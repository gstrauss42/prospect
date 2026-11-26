# Prospect

Prospect finds the opportunities a business would otherwise miss: tenders, partners, funding, regulation changes and new markets. It researches the business from its name and website and does the searching every week, so there are no alerts to set up and no people or pages to follow. The value arrives two ways: a weekly email with just the opportunities worth acting on, and a dashboard with the full picture behind them.

**In your inbox:** a short weekly email of new opportunities, best first, each with why it fits and the next step. Tell it what's useful and what isn't, and the next one is sharper.

**On your dashboard:** everything it has found, the sourced research behind it (overview, products, customers, competitors, market, regulation and news), and the controls to tune it.

![The Prospect dashboard for Personio, with 14 opportunities found from public sources](docs/assets/hero.jpg)

*From a live run on Personio, using public sources only.*

## The problem

**Who it's for:** owner-managers and business-development leads at South African B2B companies with roughly 10–200 people and no bid, partnerships or market-intelligence team.

**The pain:** staying on top of opportunities means setting up alerts, following people and companies on social media, subscribing to newsletters and checking tender portals, then filtering all of it by hand. Nobody has time for that, so tenders, partner programmes, grants and regulation changes get found late, found by a competitor, or not found at all.

**Scope:** a tuned feed of the external opportunities one business is missing, delivered to the inbox and explained on a dashboard. Not a CRM, a contact database or an internal-metrics dashboard.

## Evidence

- South Africa had about 710,000 small formal businesses in 2022 ([TIPS](https://www.tips.org.za/images/REB_The_State_of_Small_Business_in_South_Africa_2024_.pdf)). In 2025, 52.8% of small businesses were in contraction, difficulty or at risk of closing ([Absa Small Business Growth Index](https://www.absa.co.za/business/absa-small-business-growth-index-2025/)).
- Public procurement was about 15% of South Africa's GDP in FY21/22, and 76% of it is spent by provinces, municipalities and public entities rather than national government ([IMF](https://www.imf.org/-/media/files/publications/selected-issues-papers/2023/english/sipea2023041.pdf.md)). National Treasury's eTender portal notes that its own data is incomplete ([Open Contracting Data Registry](https://data.open-contracting.org/en/publication/143)). There is no single place to watch.

**What we learned:** consumer-facing businesses have few formal tenders, so watching tenders alone fails them. We changed the search planning to depend on the business model: partner and supplier programmes, expansion, awards and funds for consumer businesses; tenders and procurement for B2B. A consumer-facing test business went from 1 to 7 open opportunities.

**What we haven't done yet:** user interviews. The next step is a pilot with ten South African owner-managers who fit the persona, to measure the targets below.

## Opportunity and why now

Today's alternatives all leave the setup and the filtering to you: tender alerts match keywords, Google Alerts and social feeds are noisy, newsletters are generic, and sales- and competitive-intelligence tools each cover one slice at enterprise prices. None of them starts from an understanding of the business, and none learns from what you ignore.

If a business-development lead spends three hours a week scanning for opportunities (our estimate), that is about 150 hours a year, and it still misses most of what's out there.

What has changed is that AI models can now read websites and search results and turn them into sourced findings at a cost low enough to repeat every week. Based on the token usage in our test runs, a weekly refresh on a Claude Sonnet-class model should fit within US$10 per business per month (an estimate).

## Success

| Measure | Target |
|---|---|
| Accuracy | every opportunity links to its source; 90% of profile facts confirmed correct by the business |
| Relevance | at least 5 open, relevant opportunities in the first email; half rated worth pursuing |
| New information | at least 30% of opportunities not already known to the team |
| Tuning | the share of items marked "not for me" falls over the first month |
| Speed | first opportunities within 5 minutes of signing up (currently 2–3 minutes) |
| Cost | under US$10 per business per month |

**In scope:** researching the business, finding and explaining opportunities, the weekly email, the dashboard, and feedback that tunes both.

**Out of scope:** acting on opportunities (outreach, bid writing), CRM and contact lists, legal advice, consumers.

## How it works

1. Sign up with a business name and website.
2. Prospect reads the website and searches the web for news, competitors, market trends and regulation. It builds a profile in which every fact has a source, and you check it once and correct anything that's wrong.
3. Every week it searches for opportunities across sales, partnerships, funding, regulation, markets, events, talent and suppliers, confirms the best ones by reading their pages, and sends the email. The dashboard updates with everything it found.
4. You tune it: mark items as useful or not, or add a note such as "more government tenders", and the next email and the dashboard change. Replying to the email to give feedback is next on the roadmap.

It runs on Claude through Amazon Bedrock, in the customer's own AWS account.
