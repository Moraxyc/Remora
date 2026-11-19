fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the schema in pure Rust, so building needs no system protoc.
    let descriptors = protox::compile(["gv_pb.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
