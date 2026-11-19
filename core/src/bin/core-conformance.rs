use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("write") if args.len() == 3 => weirnote_core::conformance::write(Path::new(&args[2]))?,
        Some("migrate") if args.len() == 3 => {
            weirnote_core::conformance::migrate(Path::new(&args[2]))?
        }
        Some("extra") if args.len() == 3 => {
            weirnote_core::conformance::add_extra_fixture(Path::new(&args[2]))?
        }
        Some("sql") if args.len() == 4 => {
            weirnote_core::conformance::sql(Path::new(&args[2]), Path::new(&args[3]))?
        }
        Some("merge") if args.len() == 4 => {
            weirnote_core::conformance::merge(Path::new(&args[2]), &args[3])?
        }
        Some("import-local") if args.len() == 4 => {
            weirnote_core::conformance::import_local(Path::new(&args[2]), Path::new(&args[3]))?
        }
        Some("recovery") if args.len() == 4 => std::fs::write(
            &args[3],
            serde_json::to_vec(&weirnote_core::conformance::recovery(Path::new(&args[2])))?,
        )?,
        Some("legacy-import") if args.len() == 4 => std::fs::write(
            &args[3],
            serde_json::to_vec(&weirnote_core::conformance::legacy_import(Path::new(
                &args[2],
            )))?,
        )?,
        Some("dump") if args.len() == 4 => std::fs::write(
            &args[3],
            serde_json::to_vec_pretty(&weirnote_core::conformance::dump(Path::new(&args[2]))?)?,
        )?,
        _ => {
            return Err(
                "usage: core-conformance write|migrate <path> | dump <path> <output>".into(),
            )
        }
    }
    Ok(())
}
