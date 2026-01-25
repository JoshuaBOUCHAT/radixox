fn parse_varint_header(buf: &[u8], n: usize) -> IOResult<(usize, usize)> {
    let mut cursor = std::io::Cursor::new(&buf[..n]);
    let msg_len = prost::encoding::decode_varint(&mut cursor)
        .map_err(|e| std::io::Error::other(format!("Varint: {e}")))? as usize;

    if msg_len > MAX_MSG_SIZE {
        return Err(std::io::Error::other(format!(
            "Message too large: {msg_len} > {MAX_MSG_SIZE}"
        )));
    }

    Ok((msg_len, cursor.position() as usize))
}

/// Decode le buffer en Command
fn decode_command(buf: &[u8]) -> IOResult<Command> {
    let net_cmd =
        NetCommand::decode(buf).map_err(|e| std::io::Error::other(format!("Proto: {e}")))?;

    net_cmd
        .validate()
        .map_err(|_| std::io::Error::other("Invalid command"))
}

async fn read_command(stream: &mut TcpStream, buf: &mut Vec<u8>) -> IOResult<Command> {
    // 1. On récupère le buffer (ownership)
    let tmp = std::mem::take(buf);

    // 2. Première lecture (on peut lire le header + une partie du payload)
    let (res, tmp) = stream.read(tmp).await;
    let n = res?;

    if n == 0 {
        *buf = tmp;
        return Err(std::io::ErrorKind::ConnectionReset.into());
    }

    // 3. Analyse du header
    let (msg_len, varint_len) = parse_varint_header(&tmp, n)?;
    let total_expected = varint_len + msg_len;

    // 4. On vérifie s'il nous en manque
    let mut tmp = tmp;
    if n < total_expected {
        // --- LA MAGIE MONOIO ---
        // On crée une "vue" sur le buffer qui commence à l'index 'n'
        // et s'arrête à 'total_expected'
        let slice = tmp.slice_mut(n..total_expected);

        // On passe la slice à read_exact. Monoio va écrire uniquement
        // dans la zone vide à la suite de ce qu'on a déjà lu.
        let (res, slice) = stream.read_exact(slice).await;
        res?;

        // On récupère le Vec d'origine
        tmp = slice.into_inner();
    }

    // 5. Décodage (Zero-copy via slice)
    let cmd = decode_command(&tmp[varint_len..total_expected])?;

    // 6. On rend le buffer pour la prochaine itération
    // Optionnel : on pourrait vider le buffer ici ou gérer le "surplus" lu
    *buf = tmp;
    buf.clear();

    Ok(cmd)
}
