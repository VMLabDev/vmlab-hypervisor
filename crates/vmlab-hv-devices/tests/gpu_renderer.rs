use std::path::PathBuf;
use std::sync::Arc;
use vmlab_hv_devices::{
    DeviceError, Snapshot, VirtioDevice,
    virtio::gpu::{BlobMapper, Gpu, GpuMode},
};

struct Mapper;
impl BlobMapper for Mapper {
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
fn renderer_initializes_on_selected_node() {
    let Some(node) = std::env::var_os("VMLAB_HV_TEST_RENDER_NODE") else {
        eprintln!("SKIP: set VMLAB_HV_TEST_RENDER_NODE to exercise the host renderer");
        return;
    };
    let gpu = Gpu::new("probe");
    gpu.enable_3d(GpuMode::Virgl, PathBuf::from(node), Arc::new(Mapper))
        .unwrap();
    assert_ne!(gpu.features() & 1, 0);
    let mut config = [0; 16];
    gpu.read_config(0, &mut config);
    assert!(u32::from_le_bytes(config[12..16].try_into().unwrap()) >= 2);
    assert!(
        gpu.save_state()
            .unwrap_err()
            .to_string()
            .contains("accelerated GPU")
    );
}
