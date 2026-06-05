//! Friendly, DNS-safe name generation for auto-named groups (ADR-0030).
//!
//! Under virtual-host routing a group name doubles as its subdomain, so a
//! generated name must be a valid DNS label: lowercase, `[a-z0-9-]`,
//! `1..=63` chars, no leading/trailing hyphen. We generate `adjective-noun`
//! pairs — memorable and token-efficient (~2 tokens), per the AI-friendly
//! identifier ethos of ADR-0016, and far better than a ULID in a hostname.
//!
//! 128 adjectives × 128 nouns = 16,384 bare combinations; collisions fall
//! back to a numeric disambiguator (ADR-0030), so the effective space is
//! unbounded and correctness never depends on the list size. Both words are
//! single lowercase ASCII tokens, so `{adj}-{noun}` is always a valid
//! single-hyphen DNS label.

use rand::Rng;

/// Max length of a single DNS label.
pub const MAX_LABEL_LEN: usize = 63;

const ADJECTIVES: &[&str] = &[
    "swift", "calm", "amber", "brave", "bright", "clever", "cosmic", "crisp", "dapper", "eager",
    "fancy", "gentle", "glad", "golden", "happy", "jolly", "keen", "lively", "lucky", "merry",
    "mellow", "noble", "plucky", "proud", "quiet", "rapid", "ruby", "sage", "scarlet", "shiny",
    "sleek", "snappy", "solar", "spry", "sterling", "sunny", "teal", "tidy", "vivid", "witty",
    "zesty", "azure", "breezy", "frosty", "hardy", "ivory", "jaunty", "mighty", "agile", "ample",
    "balmy", "bold", "brisk", "chipper", "classic", "clean", "cobalt", "cozy", "dandy", "daring",
    "deft", "dewy", "downy", "dreamy", "earnest", "easy", "fleet", "fond", "frank", "fresh",
    "gallant", "giddy", "graceful", "grand", "hearty", "honest", "humble", "jade", "kind",
    "limber", "lithe", "loyal", "lush", "marble", "modest", "nimble", "opal", "peppy", "placid",
    "polished", "prime", "pure", "quick", "regal", "robust", "rosy", "rugged", "rustic", "serene",
    "sharp", "silver", "smart", "smooth", "snug", "sober", "spirited", "steady", "stoic", "stout",
    "sturdy", "suave", "sunlit", "sweet", "tender", "trusty", "upbeat", "urbane", "valiant",
    "velvet", "verdant", "warm", "vibrant", "wise", "wistful", "wry", "zealous", "zen", "plush",
];

const NOUNS: &[&str] = &[
    "otter", "finch", "falcon", "willow", "cedar", "comet", "ember", "river", "meadow", "badger",
    "heron", "sparrow", "lynx", "beacon", "canyon", "delta", "fjord", "glade", "grove", "harbor",
    "isle", "kestrel", "lagoon", "maple", "nimbus", "oak", "pine", "quartz", "reef", "summit",
    "thicket", "vale", "walrus", "yarrow", "zephyr", "basin", "brook", "cove", "dune", "fern",
    "gull", "hollow", "inlet", "jetty", "knoll", "ledge", "marsh", "ridge", "acorn", "alder",
    "arbor", "aspen", "aurora", "birch", "bison", "bluff", "bramble", "breeze", "cliff", "cloud",
    "crane", "creek", "crest", "dahlia", "dale", "dawn", "dell", "drift", "eagle", "egret", "elm",
    "fox", "gale", "garnet", "geyser", "glacier", "gorge", "hawk", "hazel", "hedge", "heath",
    "hill", "ibis", "juniper", "lake", "lark", "laurel", "lily", "lotus", "mantis", "marlin",
    "mesa", "mist", "moss", "moth", "oasis", "onyx", "orchid", "osprey", "owl", "peak", "pebble",
    "petal", "plover", "pond", "poppy", "prairie", "quail", "raven", "robin", "sable", "shore",
    "sierra", "slope", "sorrel", "spruce", "stork", "stream", "tarn", "tundra", "vista", "wren",
    "glen", "beck", "fell", "holt", "wold", "moor",
];

/// Generate one `adjective-noun` candidate. Always a valid DNS label.
pub fn generate() -> String {
    let mut rng = rand::rng();
    let adj = ADJECTIVES[rng.random_range(0..ADJECTIVES.len())];
    let noun = NOUNS[rng.random_range(0..NOUNS.len())];
    format!("{adj}-{noun}")
}

/// True iff `name` is a valid DNS label usable as a subdomain (ADR-0030):
/// lowercase ASCII alphanumerics and hyphens, `1..=63` chars, no
/// leading/trailing hyphen.
pub fn is_valid_label(name: &str) -> bool {
    let len = name.len();
    if len == 0 || len > MAX_LABEL_LEN {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_names_are_valid_dns_labels() {
        for _ in 0..500 {
            let name = generate();
            assert!(
                is_valid_label(&name),
                "generated name {name:?} is not a valid DNS label"
            );
            assert!(name.contains('-'), "expected adjective-noun shape: {name}");
        }
    }

    #[test]
    fn generate_covers_more_than_one_value() {
        // Not a strict distribution test — just that it isn't constant.
        let seen: HashSet<String> = (0..50).map(|_| generate()).collect();
        assert!(seen.len() > 1, "generator produced only {seen:?}");
    }

    #[test]
    fn word_lists_are_sized_and_unique() {
        // Guards the documented combination count and catches accidental
        // duplicates that would silently shrink the namespace.
        assert_eq!(ADJECTIVES.len(), 128, "adjective count drifted");
        assert_eq!(NOUNS.len(), 128, "noun count drifted");
        assert_eq!(
            ADJECTIVES.iter().collect::<HashSet<_>>().len(),
            128,
            "duplicate adjective"
        );
        assert_eq!(
            NOUNS.iter().collect::<HashSet<_>>().len(),
            128,
            "duplicate noun"
        );
        // Every word on its own is a valid label fragment.
        for w in ADJECTIVES.iter().chain(NOUNS.iter()) {
            assert!(is_valid_label(w), "word {w:?} is not DNS-safe");
        }
    }

    #[test]
    fn label_validation_rules() {
        assert!(is_valid_label("swift-otter"));
        assert!(is_valid_label("amber-finch-2"));
        assert!(is_valid_label("g1"));
        assert!(!is_valid_label(""));
        assert!(!is_valid_label("-leading"));
        assert!(!is_valid_label("trailing-"));
        assert!(!is_valid_label("Upper"));
        assert!(!is_valid_label("under_score"));
        assert!(!is_valid_label("has space"));
        assert!(!is_valid_label("dot.dot"));
        assert!(!is_valid_label(&"x".repeat(MAX_LABEL_LEN + 1)));
    }
}
