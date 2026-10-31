fn main() {
    pkg_config::Config::new()
        .atleast_version("1.3.0")
        .probe("virglrenderer")
        .expect("virtio-gpu requires virglrenderer >= 1.3.0");
    pkg_config::probe_library("egl").expect("GPU scanout requires EGL");
    pkg_config::probe_library("gbm").expect("GPU scanout requires GBM");
}
