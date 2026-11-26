//! Write sample emails (welcome, digest, password reset) to an outbox folder, as `.eml` and
//! `.html`, to look at them in a browser or mail client. Sends nothing.
//!
//! `cargo run -p mailer --example preview -- /tmp/prospect-email-preview`

use std::path::PathBuf;

use mailer::{Config, Delivery, Mailer, digest, password_reset, welcome};
use model::Opportunity;

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "prospect-email-preview".into()),
    );
    let cfg = Config {
        api_key: None,
        domain: String::new(),
        sender: String::new(),
        base_urls: vec![],
        outbox_dir: dir.clone(),
        public_url: "https://prospect.example.org".into(),
        app_name: "Prospect".into(),
    };
    let m = Mailer::new(cfg.clone());
    let items = vec![
        Opportunity {
            url: "https://example.org/bursary".into(),
            title: "Bryan Montgomery Bursary: R35 000 towards culinary tuition".into(),
            organization: "SA Chefs Association".into(),
            category: "funding".into(),
            why: "You're training to open a small restaurant, and this covers a year of culinary school in Cape Town.".into(),
            deadline: Some("2027-01-15".into()),
            value: "R35 000".into(),
            score: 94,
            ..Default::default()
        },
        Opportunity {
            url: "https://example.org/young-chef".into(),
            title: "Young Chef of the Year 2027: open call".into(),
            organization: "World Association of Chefs' Societies".into(),
            category: "visibility".into(),
            why: "Your street-food menus are exactly the kind of cooking the regional round rewards.".into(),
            deadline: Some("2027-02-28".into()),
            value: "$5,000 prize".into(),
            score: 88,
            ..Default::default()
        },
        Opportunity {
            url: "https://example.org/market".into(),
            title: "Oranjezicht City Farm Market: new food traders wanted".into(),
            organization: "OZCF Market".into(),
            category: "sales".into(),
            why: "A Saturday stall puts your food in front of a few thousand people a week.".into(),
            value: "Stall from R450 a day".into(),
            score: 81,
            ..Default::default()
        },
    ];
    let emails = [
        welcome(&cfg, "Acme Cleaning", "gabriel@example.com"),
        digest(
            &cfg,
            "Acme Cleaning",
            "gabriel@example.com",
            &items,
            18,
            "Cooking",
            "#c2410c",
        ),
        password_reset(
            &cfg,
            "Acme Cleaning",
            "gabriel@example.com",
            "https://prospect.example.org/reset?token=3f9c2a",
        ),
    ];
    for e in &emails {
        match m.send(e) {
            Ok(Delivery::Outbox { path }) => println!("{}", path.with_extension("html").display()),
            Ok(other) => println!("{other:?}"),
            Err(e) => eprintln!("failed: {e}"),
        }
    }
}
