fn main() {
    let now = chrono::offset::Local::now().format("%m%d%Y.%H%M");
    println!("cargo:rustc-env=SHRS_VERSION=v{now}-dev");
}
