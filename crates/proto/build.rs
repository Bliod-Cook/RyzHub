fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../../proto";
    let file = "../../proto/agentplatform/v1/wire.proto";
    println!("cargo:rerun-if-changed={root}");
    let descriptor = protox::compile([file], [root])?;
    prost_build::Config::new()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .field_attribute(".agentplatform.v1.RunState.read_only", "#[serde(default)]")
        .compile_fds(descriptor)?;
    Ok(())
}
