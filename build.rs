use std::io::Result;
fn main() -> Result<()> {
    tonic_prost_build::configure()
        .build_server(false)
        .compile_protos(
            &["grpc/router.proto", "grpc/lightning.proto"],
            &["grpc/"],
        )?;
    Ok(())
}
