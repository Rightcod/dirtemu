//! Command scheduling for compute-emulated mesh groups.

use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::{HostList, HostPipeline, State};
use crate::mesh_fallback::{INDIRECT_GROUP_LIMIT, MAX_PRIMITIVES};
use crate::pipeline::host::MeshFallbackPipeline;

pub(super) struct Workspace {
    buffer: ID3D12Resource,
    bytes: u64,
}

const DIRECT_GROUP_LIMIT: u32 = 65535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectSchedule {
    NoOp,
    Draw { vertices: u32, instances: u32 },
}

pub(super) fn groups_supported(x: u32, y: u32, z: u32) -> bool {
    x <= DIRECT_GROUP_LIMIT && y == 1 && z == 1
}

fn direct_schedule(groups: u32, max_primitives: u32) -> Option<DirectSchedule> {
    if groups == 0 {
        return Some(DirectSchedule::NoOp);
    }
    if groups > DIRECT_GROUP_LIMIT || max_primitives > MAX_PRIMITIVES {
        return None;
    }
    Some(DirectSchedule::Draw {
        vertices: max_primitives.checked_mul(3)?,
        instances: groups,
    })
}

fn workspace_size(group_bytes: u32, groups: u32) -> Option<u64> {
    let bytes = u64::from(group_bytes).checked_mul(u64::from(groups))?;
    (bytes != 0 && bytes <= i32::MAX as u64).then_some(bytes)
}

pub(super) fn indirect_supported(max: u32, counted: bool) -> bool {
    max == 1 && !counted
}

pub(super) fn title_compute(compute: bool, mesh_fallback: bool) -> bool {
    compute && !mesh_fallback
}

impl State {
    fn mesh_workspace(
        &mut self,
        list: &HostList,
        p: &HostPipeline,
        fallback: &MeshFallbackPipeline,
        groups: u32,
    ) -> Option<ID3D12Resource> {
        let bytes = workspace_size(fallback.group_bytes, groups)?;
        let key = p.pso.as_raw() as usize;
        if self
            .mesh_workspaces
            .get(&key)
            .is_none_or(|workspace| workspace.bytes < bytes)
        {
            let buffer = crate::host::host()?.uav_buffer(bytes)?;
            if let Some(old) = self
                .mesh_workspaces
                .insert(key, Workspace { buffer, bytes })
            {
                list.keep(&old.buffer);
            }
        }
        let workspace = self.mesh_workspaces.get(&key)?;
        list.keep(&workspace.buffer);
        Some(workspace.buffer.clone())
    }

    fn restore_topology(&self, list: &HostList) {
        if !self.topology_set || self.topology == 0 {
            return;
        }
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(
                    super::host_topology(self.topology) as i32
                ))
        }
    }

    pub(super) fn dispatch_mesh_fallback(
        &mut self,
        list: &HostList,
        p: &HostPipeline,
        fallback: &MeshFallbackPipeline,
        groups: u32,
    ) -> bool {
        let Some(schedule) = direct_schedule(groups, fallback.max_primitives) else {
            static WARN: std::sync::Once = std::sync::Once::new();
            WARN.call_once(|| {
                tracing::warn!(
                    groups,
                    max_primitives = fallback.max_primitives,
                    "direct mesh fallback dispatch exceeds host limits"
                )
            });
            return false;
        };
        let DirectSchedule::Draw {
            vertices,
            instances,
        } = schedule
        else {
            return true;
        };
        let Some(workspace) = self.mesh_workspace(list, p, fallback, groups) else {
            return false;
        };
        self.mesh_group_base = 0;
        if !self.bind(list, p, true) {
            return false;
        }
        let gpu = unsafe { workspace.GetGPUVirtualAddress() };
        let common = D3D12_RESOURCE_STATE_COMMON;
        let unordered = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        unsafe {
            list.list.ResourceBarrier(&[
                mesh_uav_barrier(),
                crate::host::transition(&workspace, common, unordered),
            ]);
            list.list
                .SetComputeRootUnorderedAccessView(fallback.output_uav, gpu);
            list.list.Dispatch(groups, 1, 1);
            list.list.ResourceBarrier(&[
                mesh_uav_barrier(),
                crate::host::transition(&workspace, unordered, read),
            ]);
        }
        let bound = self.bind(list, p, false);
        if bound {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("mesh compute fallback {groups}"), false));
            self.mesh_scissor(list, true);
            unsafe {
                list.list
                    .SetGraphicsRootShaderResourceView(fallback.output_srv, gpu);
                list.list
                    .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                list.list.DrawInstanced(vertices, instances, 0, 0);
                list.list
                    .ResourceBarrier(&[crate::host::transition(&workspace, read, common)]);
            }
            self.mesh_scissor(list, false);
            self.restore_topology(list);
        } else {
            unsafe {
                list.list
                    .ResourceBarrier(&[crate::host::transition(&workspace, read, common)]);
            }
        }
        bound
    }

    pub(super) fn execute_mesh_fallback(
        &mut self,
        list: &HostList,
        p: &HostPipeline,
        fallback: &MeshFallbackPipeline,
        arguments: &ID3D12Resource,
        stride: u32,
    ) -> bool {
        let Some(dispatch_signature) = super::host_signature(2, stride) else {
            return false;
        };
        let Some(draw_signature) = super::host_signature(0, fallback.group_bytes) else {
            return false;
        };
        let Some(workspace) = self.mesh_workspace(list, p, fallback, INDIRECT_GROUP_LIMIT) else {
            return false;
        };
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| {
            tracing::warn!(
                INDIRECT_GROUP_LIMIT,
                "mesh fallback indirect records with more groups run empty; multi-record and count-buffer forms are refused"
            )
        });
        self.mesh_group_base = 0;
        if !self.bind(list, p, true) {
            return false;
        }
        let gpu = unsafe { workspace.GetGPUVirtualAddress() };
        let common = D3D12_RESOURCE_STATE_COMMON;
        let unordered = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let read =
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
        unsafe {
            list.list.ResourceBarrier(&[
                mesh_uav_barrier(),
                crate::host::transition(&workspace, common, unordered),
            ]);
            list.list
                .SetComputeRootUnorderedAccessView(fallback.output_uav, gpu);
            list.list
                .ExecuteIndirect(&dispatch_signature, 1, arguments, 0, None, 0);
            list.list.ResourceBarrier(&[
                mesh_uav_barrier(),
                crate::host::transition(&workspace, unordered, read),
            ]);
        }
        let bound = self.bind(list, p, false);
        if bound {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| {
                self.op_text(
                    format!("mesh compute fallback {INDIRECT_GROUP_LIMIT}"),
                    false,
                )
            });
            self.mesh_scissor(list, true);
            unsafe {
                list.list
                    .SetGraphicsRootShaderResourceView(fallback.output_srv, gpu);
                list.list
                    .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                list.list.ExecuteIndirect(
                    &draw_signature,
                    INDIRECT_GROUP_LIMIT,
                    &workspace,
                    0,
                    Some(arguments),
                    0,
                );
                list.list
                    .ResourceBarrier(&[crate::host::transition(&workspace, read, common)]);
            }
            self.mesh_scissor(list, false);
            self.restore_topology(list);
        } else {
            unsafe {
                list.list
                    .ResourceBarrier(&[crate::host::transition(&workspace, read, common)]);
            }
        }
        bound
    }
}

fn mesh_uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_compute_reads_the_title_graphics_roots() {
        assert!(!title_compute(true, true));
        assert!(title_compute(true, false));
        assert!(!title_compute(false, false));
    }

    #[test]
    fn indirect_subset_is_explicit() {
        assert!(indirect_supported(1, false));
        assert!(!indirect_supported(0, false));
        assert!(!indirect_supported(2, false));
        assert!(!indirect_supported(1, true));
    }

    #[test]
    fn mesh_dispatches_are_one_dimensional() {
        assert!(groups_supported(1, 1, 1));
        assert!(groups_supported(DIRECT_GROUP_LIMIT, 1, 1));
        assert!(!groups_supported(DIRECT_GROUP_LIMIT + 1, 1, 1));
        assert!(!groups_supported(1, 2, 1));
        assert!(!groups_supported(1, 1, 2));
        assert!(!groups_supported(1, 0, 1));
    }

    #[test]
    fn direct_dispatch_is_one_draw_for_every_legal_group_count() {
        assert_eq!(direct_schedule(0, 4), Some(DirectSchedule::NoOp));
        for groups in [1, INDIRECT_GROUP_LIMIT, INDIRECT_GROUP_LIMIT + 1, 65_535] {
            assert_eq!(
                direct_schedule(groups, 4),
                Some(DirectSchedule::Draw {
                    vertices: 12,
                    instances: groups,
                })
            );
        }
        assert_eq!(direct_schedule(65_536, 4), None);
        assert_eq!(direct_schedule(1, u32::MAX), None);
    }

    #[test]
    fn max_primitives_sets_three_vertices_per_group() {
        for max_primitives in [0, 1, 2, 64, MAX_PRIMITIVES] {
            assert_eq!(
                direct_schedule(7, max_primitives),
                Some(DirectSchedule::Draw {
                    vertices: max_primitives * 3,
                    instances: 7,
                })
            );
        }
        assert_eq!(direct_schedule(1, MAX_PRIMITIVES + 1), None);
    }

    #[test]
    fn workspace_size_is_checked_and_covers_the_whole_dispatch() {
        assert_eq!(workspace_size(112, 1), Some(112));
        assert_eq!(
            workspace_size(112, INDIRECT_GROUP_LIMIT + 17),
            Some(116_592)
        );
        assert_eq!(workspace_size(0, 1), None);
        assert_eq!(workspace_size(112, 0), None);
        assert_eq!(workspace_size(u32::MAX, 2), None);
        assert_eq!(workspace_size(32_768, 65_535), Some(2_147_450_880));
        assert_eq!(workspace_size(32_769, 65_535), None);
    }
}
