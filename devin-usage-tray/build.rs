// build.rs — 编译期嵌入 git commit 短哈希，--version 时显示，
// 便于核对三端部署的二进制是否同源。
fn main() {
    // 优先读 deploy.sh 捎带的 GIT_HASH 文件（远端无 .git 时也能显示源 commit）
    let hash = std::fs::read_to_string("GIT_HASH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "nogit".into());
    println!("cargo:rustc-env=GIT_HASH={hash}");
    println!("cargo:rerun-if-changed=GIT_HASH");
    println!("cargo:rerun-if-changed=../.git/HEAD");
}
