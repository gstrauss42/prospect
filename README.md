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

## Business case

### Pricing

Prospect is priced per business, per month. Refresh frequency is the upgrade lever because it drives both the value and the cost. We don't charge per opportunity, which would reward volume over relevance.

| Plan | Price | Includes |
|---|---|---|
| Starter | R899 a month (about US$49) | weekly email and dashboard, all eight focus areas |
| Growth | R2,699 a month (about US$149) | daily refresh, up to five email recipients, a monthly refresh of the research |
| Enterprise | from R8,999 a month (about US$499) | runs on Claude in the customer's own AWS account (Amazon Bedrock), which pays its own AI usage |

**Why these prices:** a business-development lead spends about 13 hours a month scanning for opportunities (three hours a week, our estimate above). At about R300 an hour that is R3,900 a month, and Starter costs 23% of it. For a US buyer at about US$70 an hour the same time is worth about US$910, and Growth costs 16% of it. One won contract more than pays for itself: a R200,000 contract at a 20% margin covers about three and a half years of Starter.

All amounts use R18 = US$1.

### Costs and unit economics

Costs per business assume Claude Sonnet-class pricing (US$3 per million input tokens, US$15 per million output tokens) and US$0.01 per web search or page read. Token counts are estimated from the usage of our live runs.

| Item | Assumption | Cost |
|---|---|---|
| One opportunity search | about 100,000 input and 15,000 output tokens, 30 searches and page reads | about US$0.83 |
| Researching the business | about 60,000 input and 8,000 output tokens, 10 searches | about US$0.40 |
| Starter, per month | 4.3 opportunity searches and one research refresh | about US$4 |
| Growth, per month | 30 opportunity searches and one research refresh | about US$25 |

Across the plan mix below, AI and search cost about US$9 per business per month. Payment processing (3%) adds about US$3 and support about US$5, so serving an average customer costs about US$17 against about US$97 of revenue: a **gross margin of about 82%**, and about US$79 a month towards fixed costs.

Hosting and email cost about US$120 a month, and one small server handles well over 1,000 businesses on a weekly refresh. Ongoing development is costed as one full-time developer at about R55,000 (US$3,000) a month, so fixed costs are about **US$3,120 a month**.

### Projections

| Assumption | Value |
|---|---|
| Plan mix | 70% Starter, 25% Growth, 5% Enterprise, an average of about US$97 per customer per month |
| Churn | 5% a month, so customers stay about 20 months |
| Cost to win a customer | about US$300 (content, outreach through business associations, partner referrals) |
| Free trial | 14 days, costing us about US$1.25; one in five trials converts |
| Growth | 15 new paying customers a month from month 2 |
| Sales cycle | self-serve within the trial for Starter and Growth; one to three months for Enterprise |

That gives a lifetime value of about US$1,590 per customer, about **5.3 times** the cost of winning one, and that cost is earned back in about 4 months. Acquiring 15 customers a month costs US$4,500, so while growing at that rate the business **breaks even at about 96 paying businesses, in month 9**, and needs about **US$31,000** to get there.

| Month | Paying businesses | Revenue per month | Profit per month |
|---|---|---|---|
| 3 | 29 | US$2,800 | −US$5,300 |
| 6 | 68 | US$6,500 | −US$2,200 |
| 9 | 101 | US$9,700 | +US$400 |
| 12 | 129 | US$12,500 | +US$2,700 |

### Risks and sensitivity

| Scenario | What changes | Break-even |
|---|---|---|
| Base case | as above | about 96 businesses, month 9 |
| AI costs double | AI and search cost about US$18 per business | about 108 businesses |
| Prices 30% lower | average revenue about US$68 | about 148 businesses |
| Twice the cost to win a customer | US$600 per customer | about 153 businesses |
| Relevance below target | churn rises to 8%, lifetime value falls to about US$990 | about 96 businesses, a month later |
| All four at once | | not reached at 15 new customers a month; needs about 23 |

The risks behind these scenarios, and what limits them:

- **Relevance and trust.** If fewer than half the opportunities are worth pursuing, churn rises and the business stays small. Prospect reads the top pages to confirm opportunities are open, links every item to its source and learns from feedback, and the pilot measures relevance before acquisition spending grows.
- **Willingness to pay.** A 30% lower price is the biggest single swing. The pilot tests the prices before they're used to plan spending.
- **AI pricing.** Doubling model costs moves break-even by about 12 businesses. Smaller models can handle the planning steps, and Enterprise customers bring their own AWS account.
- **Access to sources.** Websites can block automated reading. Prospect draws on many sources, so losing one has little effect.
- **Data protection (POPIA and GDPR).** Prospect researches public information only.
