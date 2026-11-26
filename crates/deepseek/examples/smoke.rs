//! Live smoke test against the real DeepSeek API (costs well under a cent):
//! `DEEPSEEK_API_KEY=… cargo run -p deepseek --example smoke`
use deepseek::{ChatRequest, Client, Config, SearchRequest};

fn main() {
    let cfg = Config::from_env().expect("set DEEPSEEK_API_KEY");
    let c = Client::new(cfg);
    println!("balance: ${:.2}", c.balance_usd().expect("balance"));
    let v = c
        .chat_json_value(
            &ChatRequest::new(
                "smoke",
                "Reply in JSON: {\"word\": \"<one word>\"}",
                "One word for opportunity?",
            )
            .max_tokens(50),
        )
        .expect("chat");
    println!("chat: {v}");
    let results = c
        .search(&SearchRequest::new(
            "open call for chefs competition 2027",
            3,
        ))
        .expect("search");
    for r in &results {
        println!("search: [{:.2}] {} — {}", r.rerank_score, r.title, r.url);
    }
    if let Some(r) = results.first() {
        match c.fetch(&r.url) {
            Ok(p) => println!("fetch: {} ({} chars)", p.title, p.content.len()),
            Err(e) => println!("fetch failed (acceptable): {e}"),
        }
    }
    let u = c.usage();
    println!(
        "usage: chats={} in={} cached={} out={} searches={} fetches={} cost=${:.5}",
        u.chat_calls,
        u.input_tokens,
        u.cached_tokens,
        u.output_tokens,
        u.searches,
        u.fetches,
        u.cost_usd
    );
    for call in &u.calls {
        println!(
            "  {} {} status={} attempts={} {}ms {}",
            call.kind, call.stage, call.status, call.attempts, call.ms, call.note
        );
    }
}
