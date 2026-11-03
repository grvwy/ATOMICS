//! In a browser, the ATOMICS web client. On the build machine, `atomics-web manifest DIST BUILT_FROM`
//! writes the bundle's `manifest.json` (scripts/build-web.sh runs it).

#[cfg(target_arch = "wasm32")]
fn main() {
    atomics_web::web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [command, dist, built_from] if command == "manifest" => {
            match atomics_web::manifest::write(std::path::Path::new(dist), built_from) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("atomics-web manifest: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("atomics-web runs in a browser; scripts/build-web.sh builds it.");
            eprintln!("usage: atomics-web manifest DIST BUILT_FROM");
            ExitCode::from(2)
        }
    }
}
