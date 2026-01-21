use slotmap::{DefaultKey, SlotMap};

use crate::oxidart::nodes::{HugeNode, LargeNode, MediumNode, SmallNode};

struct OxidART {
    map_small: SlotMap<DefaultKey, SmallNode>,
    map_medium: SlotMap<DefaultKey, MediumNode>,
    map_large: SlotMap<DefaultKey, LargeNode>,
    map_huge: SlotMap<DefaultKey, HugeNode>,
    root: HugeNode,
}
