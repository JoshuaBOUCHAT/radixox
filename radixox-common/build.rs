fn main() -> std::io::Result<()> {
    // On dit à prost de compiler le fichier qu'on a créé
    // Le premier argument est le chemin vers le .proto
    // Le deuxième est le dossier qui contient les imports (ici le dossier proto lui-même)
    let mut config = prost_build::Config::new();
    config.bytes(&["."]);

    config.compile_protos(&["src/proto/messages.proto"], &["src/proto/"])?;
    Ok(())
}
