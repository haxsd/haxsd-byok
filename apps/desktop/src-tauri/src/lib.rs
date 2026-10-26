mod ca_install;
mod desktop;
#[cfg(not(dev))]
mod frontend;
mod resource_limits;
mod startup;
mod tray;

// mimalloc 在释放时主动向操作系统归还内存,避免 glibc 保留页导致
// 长时间运行后的空闲内存无法回落到启动水平。
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub fn run() -> std::process::ExitCode {
    desktop::run()
}
