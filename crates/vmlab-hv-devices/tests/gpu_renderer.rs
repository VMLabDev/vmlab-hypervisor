use std::path::PathBuf;
use std::sync::Arc;
use vmlab_hv_devices::{
    DeviceError, Snapshot, VirtioDevice,
    virtio::gpu::{BlobMapper, Gpu, GpuMode},
};

struct Mapper;
impl BlobMapper for Mapper {
    fn size(&self) -> u64 {
        4 << 30
    }
    fn set_base(&self, _: Option<u64>) -> Result<(), DeviceError> {
        Ok(())
    }
    fn map(&self, _: u32, _: u64, _: u64, _: u64) -> Result<(), DeviceError> {
        Err(DeviceError::Other("no VM in renderer probe".into()))
    }
    fn unmap(&self, _: u32) -> Result<(), DeviceError> {
        Ok(())
    }
}

#[test]
#[cfg_attr(
    vmlab_ci_no_host_assets,
    ignore = "requires an owner-selected DRM render node"
)]
fn renderer_initializes_on_selected_node() {
    let Some(node) = std::env::var_os("VMLAB_HV_TEST_RENDER_NODE") else {
        eprintln!("SKIP: set VMLAB_HV_TEST_RENDER_NODE to exercise the host renderer");
        return;
    };
    let venus = std::env::var("VMLAB_HV_TEST_GPU_MODE").is_ok_and(|mode| mode == "venus");
    let mode = if venus {
        GpuMode::Venus
    } else {
        GpuMode::Virgl
    };
    let gpu = Gpu::new("probe");
    gpu.enable_3d(mode, PathBuf::from(node), Arc::new(Mapper))
        .unwrap();
    let accelerated = 1 | (1 << 3) | (1 << 4);
    assert_eq!(gpu.features() & accelerated, accelerated);
    let mut config = [0; 16];
    gpu.read_config(0, &mut config);
    let capsets = u32::from_le_bytes(config[12..16].try_into().unwrap());
    assert!(capsets >= if venus { 3 } else { 2 });
    assert!(
        gpu.save_state()
            .unwrap_err()
            .to_string()
            .contains("accelerated GPU")
    );
}
