fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../../proto";
    let file = "../../proto/agentplatform/v1/wire.proto";
    println!("cargo:rerun-if-changed={root}");
    let descriptor = protox::compile([file], [root])?;
    prost_build::Config::new()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        // Client caches outlive schema versions: fields added later default when absent.
        .message_attribute(".", "#[serde(default)]")
        .compile_fds(descriptor)?;
    Ok(())
}
