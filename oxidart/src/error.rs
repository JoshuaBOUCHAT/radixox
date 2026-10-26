#[derive(Debug)]
pub enum TypeError {
    ValueNotSet,
    NotAInt,
    /// Score NaN (argument ou résultat).
    NotAFloat,
    /// Table des poignées pleine (`RADIXOX_HANDLES_MAX`).
    OutOfMemory,
}
