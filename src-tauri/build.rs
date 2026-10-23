fn main() {
    // 图标换版后必须让本脚本重跑，否则 embed-resource 生成的 resource.lib 不会更新，
    // exe 里留的仍然是上一版图标（实测：只改 icons/ 再构建，PE 资源字节无变化）。
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}
