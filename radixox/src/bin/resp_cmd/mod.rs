//! Commandes liées au runtime monoio (tâches, fichiers, registre Pub/Sub).
//! Le reste vit dans la bibliothèque : `radixox::exec`.

pub(crate) mod bgsave;
pub(crate) mod delayed;
pub(crate) mod pub_sub;
pub(crate) mod shutdown;

pub(crate) use radixox::exec::{auth, client, scan};
