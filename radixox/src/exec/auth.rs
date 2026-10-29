//! AUTH par mot de passe unique (`RADIXOX_PASSWORD`), sémantique Redis
//! `requirepass` : tant que la connexion n'est pas authentifiée, toute
//! commande sauf AUTH et QUIT reçoit `-NOAUTH`. Pas d'ACL : le seul
//! utilisateur est `default`.

#[allow(unused_imports)]
use crate::prelude::*;

use radixox_lib::shared_frame::SharedFrame as Frame;

/// Mot de passe configuré ; `None` (variable absente ou vide) : pas d'AUTH.
pub fn password() -> Option<&'static [u8]> {
    radixox_lib::sys::env_bytes("RADIXOX_PASSWORD").filter(|v| !v.is_empty())
}

pub fn noauth() -> Frame {
    Frame::Error("NOAUTH Authentication required.".into())
}

/// AUTH [user] pass. Un échec ne retire pas une authentification déjà
/// acquise (comme Redis).
pub fn cmd_auth(user: Option<&[u8]>, pass: &[u8], authed: &mut bool) -> Frame {
    let Some(expected) = password() else {
        return Frame::Error(
            "ERR AUTH <password> called without any password configured for the default user. \
             Are you sure your configuration is correct?"
                .into(),
        );
    };
    let user_ok = user.is_none_or(|u| u == b"default");
    // `&` et non `&&` : la comparaison du mot de passe a lieu quel que soit l'utilisateur.
    if user_ok & ct_eq(pass, expected) {
        *authed = true;
        Frame::SimpleString("OK")
    } else {
        Frame::Error("WRONGPASS invalid username-password pair or user is disabled.".into())
    }
}

/// Égalité en temps indépendant du contenu (la longueur seule peut fuiter).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    core::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::ct_eq;

    #[test]
    fn ct_eq_matches_slice_equality() {
        let cases: [&[u8]; 6] = [b"", b"a", b"secret", b"secreT", b"secret\0", b"secre"];
        for a in cases {
            for b in cases {
                assert_eq!(ct_eq(a, b), a == b, "{a:?} {b:?}");
            }
        }
        // longueurs différant d'un multiple de 256, octets nuls
        assert!(!ct_eq(&[0; 256], b""));
        assert!(!ct_eq(&[0; 65536], b""));
    }
}
