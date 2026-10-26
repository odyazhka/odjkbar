// Ни nasm, ни C-компилятора для сборки самого бинаря не нужно.
// Скрипт только говорит компоновщику: без crt1.o, без libc, статический не-PIE файл.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for a in ["-nostartfiles", "-nostdlib", "-static", "-no-pie"] {
        println!("cargo:rustc-link-arg-bins={a}");
    }
}
