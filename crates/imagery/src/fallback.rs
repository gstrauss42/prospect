//! Built-in background photos: one per broad kind of business, chosen once with the same pipeline
//! (live search and the vision pick) and embedded in the binary. When the live search can't find
//! or download a photo (sources down, rate-limited or blocking the server), the page still gets a
//! fitting, properly credited photo instead of a plain colour.

use model::{ThemeHint, ThemeImage};

use crate::Background;

struct Fallback {
    key: &'static str,
    /// Lower-case fragments matched against the theme's label, photo searches and mood.
    keywords: &'static [&'static str],
    bytes: &'static [u8],
    credit: &'static str,
    source_url: &'static str,
}

const LIBRARY: &[Fallback] = &[
    Fallback {
        key: "technology",
        keywords: &[
            "software",
            "saas",
            "tech",
            "data",
            "cloud",
            "cyber",
            "digital",
            "platform",
            "computer",
            "internet",
            "telecom",
            "electronic",
            "developer",
            "it services",
            "ai ",
            "artificial intelligence",
        ],
        bytes: include_bytes!("../fallback/technology.jpg"),
        credit: "Photo · CC0 · rawpixel via Openverse",
        source_url: "https://www.rawpixel.com/image/5906639/photo-image-light-desktop-wallpapers-public-domain",
    },
    Fallback {
        key: "office",
        keywords: &[
            "consult",
            "agency",
            "legal",
            "law ",
            "accounting",
            "marketing",
            "recruit",
            "staffing",
            "hr ",
            "human resources",
            "design",
            "media",
            "education",
            "training",
            "professional services",
            "business services",
        ],
        bytes: include_bytes!("../fallback/office.jpg"),
        credit: "Photo: Design by Matt · CC0 · via Openverse",
        source_url: "https://stocksnap.io/photo/home-office-QUU0AQABSN",
    },
    Fallback {
        key: "city",
        keywords: &[
            "real estate",
            "property",
            "tourism",
            "travel",
            "hotel",
            "hospitality",
            "events",
            "urban",
            "venue",
        ],
        bytes: include_bytes!("../fallback/city.jpg"),
        credit: "Photo: chenisyuan · CC BY-SA 4.0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/w/index.php?curid=15408565",
    },
    Fallback {
        key: "logistics",
        keywords: &[
            "logistic",
            "transport",
            "shipping",
            "freight",
            "courier",
            "delivery",
            "warehouse",
            "supply chain",
            "import",
            "export",
            "wholesale",
            "distribution",
            "fleet",
        ],
        bytes: include_bytes!("../fallback/logistics.jpg"),
        credit: "Photo: Petar Milošević · CC BY-SA 4.0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/wiki/File:MSC_HARMONY_III_(container_ship,_2006)_in_Port_Koper_(SIKOP,_2024).jpg",
    },
    Fallback {
        key: "manufacturing",
        keywords: &[
            "manufactur",
            "factory",
            "industrial",
            "engineering",
            "machinery",
            "chemical",
            "plastic",
            "metal",
            "automotive",
            "mining",
            "production",
            "fabrication",
        ],
        bytes: include_bytes!("../fallback/manufacturing.jpg"),
        credit: "Photo: Trougnouf · CC BY 4.0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/wiki/File:Machinery_in_the_water_treatment_plant_of_an_abandoned_steel_factory_in_Oupeye,_Belgium_(DSCF3293).jpg",
    },
    Fallback {
        key: "food",
        keywords: &[
            "food",
            "restaurant",
            "catering",
            "cooking",
            "bakery",
            "coffee",
            "beverage",
            "kitchen",
            "chef",
            "grocery",
            "meal",
        ],
        bytes: include_bytes!("../fallback/food.jpg"),
        credit: "Photo: Dietmar Rabich · CC BY-SA 4.0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/wiki/File:Melbourne_(AU),_Queen_Victoria_Market_--_2019_--_1545.jpg",
    },
    Fallback {
        key: "agriculture",
        keywords: &[
            "agricultur",
            "farm",
            "agri",
            "crop",
            "livestock",
            "forestry",
            "garden",
            "nursery",
            "vineyard",
            "wine",
            "organic",
        ],
        bytes: include_bytes!("../fallback/agriculture.jpg"),
        credit: "Photo: W.carter · CC BY-SA 4.0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/wiki/File:Rows_of_grass_in_a_field_at_G%C3%A5seberga.jpg",
    },
    Fallback {
        key: "health",
        keywords: &[
            "health",
            "medical",
            "clinic",
            "pharma",
            "biotech",
            "laborator",
            "science",
            "research",
            "dental",
            "wellness",
            "hospital",
            "care ",
        ],
        bytes: include_bytes!("../fallback/health.jpg"),
        credit: "Photo: Stoyko Sabotanov · CC BY 4.0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/w/index.php?curid=64386356",
    },
    Fallback {
        key: "retail",
        keywords: &[
            "retail",
            "shop",
            "store",
            "ecommerce",
            "e-commerce",
            "fashion",
            "clothing",
            "boutique",
            "consumer",
            "beauty",
            "furniture",
            "cleaning",
        ],
        bytes: include_bytes!("../fallback/retail.jpg"),
        credit: "Photo · CC0 · rawpixel via Openverse",
        source_url: "https://www.rawpixel.com/image/5949306/free-public-domain-cc0-photo",
    },
    Fallback {
        key: "construction",
        keywords: &[
            "construction",
            "building",
            "architect",
            "civil",
            "contractor",
            "plumbing",
            "electrical",
            "renovation",
            "housing",
            "infrastructure",
        ],
        bytes: include_bytes!("../fallback/construction.jpg"),
        credit: "Photo · CC0 · rawpixel via Openverse",
        source_url: "https://www.rawpixel.com/image/5969704/construction-site-crane",
    },
    Fallback {
        key: "energy",
        keywords: &[
            "energy",
            "solar",
            "wind",
            "renewable",
            "power",
            "electricity",
            "climate",
            "sustainab",
            "environment",
            "utilit",
            "water",
        ],
        bytes: include_bytes!("../fallback/energy.jpg"),
        credit: "Photo: Vyacheslav Argenberg · CC BY 4.0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/w/index.php?curid=130832795",
    },
    Fallback {
        key: "finance",
        keywords: &[
            "financ",
            "bank",
            "insurance",
            "invest",
            "fintech",
            "payment",
            "wealth",
            "capital",
            "lending",
            "credit",
        ],
        bytes: include_bytes!("../fallback/finance.jpg"),
        credit: "Photo: Walk around the RED DOT from Singapore · CC0 · Wikimedia Commons",
        source_url: "https://commons.wikimedia.org/w/index.php?curid=189036548",
    },
];

/// The built-in photo that best fits a theme (by keyword), or the office one when nothing matches.
pub fn fallback_background(hint: &ThemeHint) -> Background {
    let text = format!(
        " {} {} {} ",
        hint.label,
        hint.image_queries.join(" "),
        hint.mood
    )
    .to_lowercase();
    let pick = LIBRARY
        .iter()
        .map(|f| (f.keywords.iter().filter(|k| text.contains(*k)).count(), f))
        .filter(|(score, _)| *score > 0)
        .max_by_key(|(score, f)| {
            (
                *score,
                std::cmp::Reverse(LIBRARY.iter().position(|x| x.key == f.key)),
            )
        })
        .map(|(_, f)| f)
        .unwrap_or(&LIBRARY[1]);
    let (width, height) = crate::sniff(pick.bytes)
        .map(|(_, w, h)| (w, h))
        .unwrap_or((1920, 1080));
    Background {
        bytes: pick.bytes.to_vec(),
        image: ThemeImage {
            mime: "image/jpeg".into(),
            width,
            height,
            credit: pick.credit.into(),
            source_url: pick.source_url.into(),
        },
    }
}

/// The key of the photo [`fallback_background`] would pick (for tests and logs).
pub fn fallback_key(hint: &ThemeHint) -> &'static str {
    let bg = fallback_background(hint);
    LIBRARY
        .iter()
        .find(|f| f.bytes.len() == bg.bytes.len() && f.credit == bg.image.credit)
        .map(|f| f.key)
        .unwrap_or("office")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(label: &str, queries: &[&str]) -> ThemeHint {
        ThemeHint {
            label: label.into(),
            image_queries: queries.iter().map(|q| q.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn every_built_in_photo_is_a_real_wide_image() {
        for f in LIBRARY {
            let (mime, w, h) =
                crate::sniff(f.bytes).unwrap_or_else(|| panic!("{} isn't a valid image", f.key));
            assert_eq!(mime, "image/jpeg", "{}", f.key);
            assert!(w >= 1600 && w > h, "{} is {w}x{h}", f.key);
            assert!(f.credit.starts_with("Photo"), "{} has no credit", f.key);
        }
    }

    #[test]
    fn photos_match_the_kind_of_business() {
        assert_eq!(
            fallback_key(&hint("Commercial kitchens", &["restaurant kitchen"])),
            "food"
        );
        assert_eq!(
            fallback_key(&hint("Freight forwarding", &["shipping containers"])),
            "logistics"
        );
        assert_eq!(
            fallback_key(&hint("Solar installers", &["solar panels on roofs"])),
            "energy"
        );
        assert_eq!(fallback_key(&hint("Payments", &["fintech app"])), "finance");
        assert_eq!(fallback_key(&hint("Something unusual", &[])), "office");
        assert!(
            !fallback_background(&hint("", &[])).bytes.is_empty(),
            "never empty"
        );
    }
}
