# Prospect

Prospect finds the opportunities a small business would otherwise miss: tenders, partner programmes, funding, regulation changes and new markets. It is built for owner-managers of South African B2B firms with no bid or partnerships team. Unlike tender alerts or Google Alerts, it starts from your website, not from keywords you have to think of. It researches the business, searches on a schedule, checks each opportunity against its source and learns from what you keep and dismiss.

The value arrives two ways:

- **A weekly email** of new opportunities, best first, each with why it fits and the next step.
- **A dashboard** with everything it has found, the sourced research behind it (competitors, market, regulation and news) and the controls to tune it.

![The Prospect dashboard for Personio, with 14 opportunities found from public sources](docs/assets/hero.jpg)

*From a live run on Personio, an HR software company we used as a test because anyone can check the results against public information: 14 opportunities, each linked to its source. Personio isn't our target customer; to see what Thandi would get, sign up with a South African B2B firm's website.*

## 1. The problem

**Who it's for:** owner-managers and business-development leads at South African B2B companies with roughly 10–200 people and no bid, partnerships or market-intelligence team.

**In one line:** the opportunities that would grow these businesses are public, but they are spread across hundreds of buyers and channels, and nobody in the company has time to watch them all.

**Meet Thandi (an illustrative persona, not a real customer).** She runs a 35-person IT managed-services firm in Durban. She sells, signs off quotes and handles the biggest client escalations. Nobody else is responsible for finding new work, so she does it on Sunday evenings. Typical moments where it bites:

- A supplier forwards a municipal network-support tender on Monday. It closes on Wednesday, and the compulsory briefing was last week.
- A competitor posts on LinkedIn that it won SMME programme funding. Thandi never saw the call for applications.
- A client asks how a new compliance rule affects their contract. It is the first she has heard of it.

Today she checks the eTender portal and a few municipal sites, keeps Google Alerts, skims newsletters and scrolls LinkedIn and WhatsApp groups. Keyword alerts miss tenders worded differently. Google Alerts are mostly noise, and nothing says whether an item is open or fits her firm. Newsletters are the same for every reader. Leads on social media turn up by chance. She filters it all by hand.

**What that costs (estimates):** about 3 hours a week (a Sunday-evening session plus quick checks during the week), or 13 hours a month. At R300 an hour (assumption) that is about R3,900 a month. The bigger cost is the work she never finds: one R200,000 contract found too late is R40,000 of lost gross profit at a 20% margin (assumption).

### Evidence

- **There are many firms like hers.** South Africa has 68,494 small (11–50 people) and 17,397 medium (51–200 people) formal firms, 85,891 in total, from National Treasury and SARS data for 2016 ([Small Business Institute](https://www.smallbusinessinstitute.co.za/?p=2355)). Counting one-person firms too, there were about 710,000 small formal businesses in 2022 ([TIPS](https://www.tips.org.za/images/REB_The_State_of_Small_Business_in_South_Africa_2024_.pdf)).
- **They need new work but can't pay someone to find it.** In 2025, 52.8% of small businesses were in contraction, difficulty or at risk of closing ([Absa Small Business Growth Index](https://www.absa.co.za/business/absa-small-business-growth-index-2025/)).
- **The work exists, but it is scattered.** National Treasury estimates public procurement at about R1 trillion a year ([Pinsent Masons](https://www.pinsentmasons.com/out-law/analysis/south-african-procurement-reform-raises-compliance)), about 15% of GDP in FY21/22. 76% of it is spent by provinces, municipalities and public entities rather than national government ([IMF](https://www.imf.org/en/Publications/selected-issues-papers/Issues/2023/06/26/Public-Procurement-in-South-Africa-Issues-and-Reform-Options-535223)), each publishing in its own place.
- **The official source is incomplete.** National Treasury's eTender portal notes that its own data is incomplete ([Open Contracting Data Registry](https://data.open-contracting.org/en/publication/143)). There is no single place to watch.

**What we haven't done yet:** user interviews. Thandi is built from public evidence, not from a customer. We also have no pricing evidence and no measured cost to win a customer. The next step is a four-week pilot with ten South African owner-managers who fit this persona, judged against the targets and decision rule in section 4.

**Scope:** a tuned feed of the external opportunities one business is missing, delivered to the inbox and explained on a dashboard. The full in and out list is in section 4.

## 2. Opportunity and why now

### Market size (estimates)

| | Businesses | Revenue a year, if each pays our average US$97 a month |
|---|---|---|
| South Africa: formal firms with 11–200 people | 85,891 | about US$100 million (R1.8 billion) |
| Our steady state in the base plan (15 new a month, 5% churn) | 300 | about US$350,000 |
| Break-even in the base plan (section 5) | 96 (0.1% of the market) | about US$112,000 |

The count includes consumer-facing firms, which Prospect also serves, though B2B comes first. The United States is the later market, once South Africa proves the model.

### Existing alternatives

Businesses piece it together from the National Treasury [eTender portal](https://www.etenders.gov.za/), tender alert services such as [Leads 2 Business](https://www.l2b.co.za/), [Google Alerts](https://www.google.com/alerts), newsletters, LinkedIn, sales and competitive-intelligence tools (ZoomInfo, Apollo.io, Crayon, Klue) or a consultant. Each tool covers one channel, and a consultant charges by the day. All of them leave the setup and the filtering to you, none starts from what the business does, and none learns from what you ignore. Section 7 compares them side by side.

### Why now

- **Reading and judging web pages is now cheap.** A model can read a page, check that an opportunity is open and fits, and cite the source. A Starter month costs about US$3.60 in AI and search (estimate, section 5), under 2% of the R3,900 (about US$217) of time it replaces.
- **The rules are changing.** The Public Procurement Act was signed in 2024 ([Daily Maverick](https://www.dailymaverick.co.za/article/2024-07-28-sas-new-public-procurement-act-takes-aim-at-tenderpreneurs/)), bringing public buying under one new law that replaces rules spread across several acts. Suppliers watching by hand now have new rules and new buyer processes to learn.
- **Businesses need the revenue now.** In 2025, 52.8% of small businesses were in contraction, difficulty or at risk of closing (section 1). New work matters more than it did, and owners have less time to look for it.

## 3. How Prospect solves it

You sign up with a business name and website. In about a minute Prospect reads the site and searches the public web, then you check a short profile (what the business does, industry, stage, location, focus areas, goals) and correct anything wrong. The first opportunity search runs straight away, then weekly by default, or daily, or only when you ask. Prospect is designed to run on Claude through Amazon Bedrock; Enterprise customers run it in their own AWS account.

| Thandi's pain | What Prospect does | Where to see it in the demo |
|---|---|---|
| No time to set up alerts or follow pages | Needs only a name and a website, then one short form | Sign-up. Research takes 41–57 seconds (measured in production) |
| Found late | First search runs at sign-up, then on a schedule | First opportunities 2–3 minutes after signing up (measured in production) |
| No single place to watch | Searches the public web across eight focus areas: sales and tenders, partnerships, funding, regulation, market and expansion, events and awards, talent, suppliers | Opportunities tab. Personio: 14 results |
| Filtering by hand | Each card says why it fits (one sentence addressed to the business), the next step, deadline, value, location and fit strength. Best first | Any opportunity card |
| Is it real, and still open? | Every title links to its source. Verified means the page itself was read and confirmed open. Unsourced items are dropped | Click a title; look for Verified |
| Tenders don't suit every business | The search plan follows the business model: procurement for B2B; partner and supplier programmes, expansion, awards and funds for consumer-facing businesses | A consumer-facing test business went from 1 to 7 open opportunities after this change |
| A competitor got there first | Competitors bullseye (by how closely each overlaps), plus Market, Regulation and News tabs, all sourced | Competitors tab |
| Feeds that never learn | Save, "Not for me" (with undo), "More/Less like this", free-text Steer notes such as "more government tenders", editable profile | Dismiss one, steer, run again, compare |
| No time to log in | Weekly email digest, best first | Built. Sending needs a mail domain, so it may be off in your demo instance |

The dashboard deliberately does not show the business its own overview or products: research uses them to aim the searches, but telling a business about itself adds nothing.

**Not built yet:** each card's "AI agent" button ("Register me", "Prepare my bid") shows "Restricted in demo". The agent behind it is the next step (section 7).

## 4. Success: measurable outcomes and scope

Prospect succeeds if owner-managers act on opportunities they would otherwise have missed, and pay for that. We'll test it in a four-week pilot with ten South African owner-managers who fit the persona, and decide in week five whether to continue, change course or stop. The pilot costs under US$50 in AI and search (estimate). Nothing is spent on winning customers until a pilot passes.

### Pilot targets

These four decide the result. None has been measured yet: there are no users.

| Outcome | Target | How we measure it |
|---|---|---|
| Relevance | at least half of all opportunities rated worth pursuing | the owner rates every item in a short weekly check (worth pursuing? already knew it?), cross-checked against saves and "Not for me" in the app |
| New information | at least 30% not already known to the team | the same weekly check |
| Action | at least 5 of 10 businesses act on an opportunity: register, apply, bid or contact the organisation | the weekly check |
| Payment | at least 3 of 10 pay list price (from R899 a month) for the month after the pilot | payments received |

The business case assumes one in five self-serve trials converts. Pilot businesses are hand-picked and supported, so their bar is higher.

The product must also hit these:

| Measure | Target | How we measure it | So far |
|---|---|---|---|
| Speed | first opportunities within 5 minutes of signing up, for every pilot business | the app's timestamps | 2–3 minutes in production (research 41–57 s, first search 60–81 s) |
| Volume | at least 5 open opportunities in the first email, for at least 8 of 10 businesses | counted in each first email | 14 opportunities in the Personio run; 7 open for a consumer-facing test business, up from 1 |
| Accuracy | every item links to its source and none is past its deadline when sent; at least 90% of profile facts need no correction | an automatic check of each email; edits made at the profile check | every item links to its source; unsourced items are dropped |
| Tuning | share marked "Not for me" a third lower in week 4 than in week 1 | the feedback stored in the app | the next run uses all feedback; the effect isn't measured yet |
| Cost | AI and search under US$10 per business per month across the plan mix, which holds while one search costs under about US$0.90 | tokens and searches counted on every pilot run, at Claude Sonnet-class prices | about US$0.83 a search and US$8.80 a month across the plan mix (estimates, section 5) |

The pilot can't measure churn or contracts won. Tenders take longer than four weeks to award, so wins are tracked but don't decide the result.

### Decision after the pilot

| Result | Decision |
|---|---|
| All four pilot targets met | **Continue.** Open sign-ups with the 14-day trial and spend on acquisition at the business-case rate: about 75 trials and 15 new paying businesses a month. Any product target missed is fixed before launch. |
| Any pilot target missed, short of a stop result | **Change course.** Fix the weakest result first (the search plan if relevance is low, the price if owners rate it well but won't pay), then run a second pilot with ten new businesses. |
| Under 30% rated worth pursuing, no business acts on an opportunity, or a second pilot also misses | **Stop**, rather than spend the US$32,500 the business case needs to reach break-even. |

After launch, success means the business case holds: at least one trial in five converts, churn stays at or below 5% a month, winning a customer costs about US$300 or less, and Prospect breaks even at about 96 paying businesses in month 9 (section 5).

### Scope of this version

**In** (built and in the demo):

- Researching the business into a sourced profile the owner checks once.
- Searching the eight focus areas weekly (or daily, or on request), with a search plan set by the business model.
- Explaining each opportunity: why it fits, the next step, the deadline and a link to the source.
- The weekly email and the dashboard. The email is built, but sending needs a mail domain, so it may not reach you from the demo.
- Feedback that tunes the next run. Giving feedback by replying to the email is next.

**Out, and why:**

| Out | Why |
|---|---|
| Acting on opportunities: bids, applications, registrations, outreach | It needs the owner's documents, logins and sign-off, and the pilot must first show the opportunities are worth acting on. In the demo each card names what an agent would do ("Prepare my bid", "Register me") and says "Restricted in demo". That agent is the roadmap, not built. |
| CRM and contact lists | Owners already track deals somewhere, and lists of people carry duties under POPIA. Prospect finds organisations, not people. |
| Legal, tax or compliance advice | Regulation changes are flagged with their source, not interpreted. The owner decides with their own advisers. |
| Internal metrics | Prospect looks outward. It doesn't analyse the business's own sales or finances. |
| Consumers as customers | Prospect is sold to businesses. Consumer-facing businesses are in, with their own search plan. |
| Paywalled or login-only sources | Public pages only, so anyone can check an item by opening its link. |
| Markets outside South Africa, for now | Research isn't limited to South Africa (the hero run is on Personio, a German company), but the persona, prices and pilot are South African. The US comes later. |

## 5. Business case: costs and unit economics

All amounts use R18 = US$1. Prices exclude VAT.

| Plan | Price | Includes |
|---|---|---|
| Starter | R899 a month (about US$50) | weekly email and dashboard, all eight focus areas |
| Growth | R2,699 a month (about US$150) | daily refresh, up to five email recipients, monthly refresh of the research |
| Enterprise | from R8,999 a month (about US$500) | runs in the customer's own AWS account, which pays its own AI usage |

Refresh frequency is the upgrade lever because it drives both value and cost. No per-opportunity fees, which would reward volume over relevance. Starter costs 23% of the R3,900 a month of time it replaces, and one R200,000 contract at a 20% margin (R40,000) pays for about 44 months of it.

**Build (estimate): about US$6,200.** The demo works. A sellable version needs two months of one developer at R55,000 (US$3,056) a month: a sending domain, card billing, feedback by email reply, a security and POPIA review, and monitoring. The ten-business pilot adds under US$50 of AI usage.

**Run.** AI is costed at Claude Sonnet list prices, US$3 per million input tokens and US$15 per million output ([Anthropic](https://platform.claude.com/docs/en/about-claude/pricing)). That is cautious: smaller Claude models such as Haiku list at a fraction of this, and simple steps can move to them. Each search or page read is costed at US$0.01, the Claude web search price, though page reads are free apart from tokens. Token counts are estimates from our live runs.

| Step | Tokens and calls (estimate) | Cost |
|---|---|---|
| One opportunity search | 100,000 input, 15,000 output, 30 searches and page reads | US$0.83 |
| Researching the business | 60,000 input, 8,000 output, 10 searches | US$0.40 |
| 14-day free trial | research plus two weekly searches | US$2.05 |

Email is negligible: Amazon SES charges US$0.10 per 1,000 emails ([AWS](https://aws.amazon.com/ses/pricing/)). Card payments cost 3.2% plus R2 ([Payfast](https://payfast.io/fees/)). **Support (assumption):** 10 minutes a month for Starter, 20 for Growth and an hour for Enterprise, at US$20 an hour.

| Per business, per month (estimates) | Starter | Growth | Enterprise | Blended (70/25/5) |
|---|---|---|---|---|
| Revenue | US$49.94 | US$149.94 | US$499.94 | US$97.44 |
| AI and search | US$3.57 | US$25.15 | paid by customer | US$8.78 |
| Payment fees | US$1.71 | US$4.91 | US$16.11 | US$3.23 |
| Support | US$3.33 | US$6.67 | US$20.00 | US$5.00 |
| **Contribution** | **US$41.33** | **US$113.21** | **US$463.83** | **US$80.43** |
| **Margin** | **83%** | **76%** | **93%** | **83%** |

Starter's AI is 4.3 weekly searches plus its sign-up research spread over a 20-month life. Growth's is 30 daily searches plus one research refresh.

**Why the margin is plausible.** AI is the only cost that grows with use, and refresh frequency, which we price, sets it. Growth costs about seven times as much in AI as Starter and is priced at three times as much, so its margin is the thinnest. If AI costs doubled, the blended margin would still be about 74%.

**Fixed costs: about US$3,180 a month.** One developer (US$3,056) plus hosting at about US$120 (assumption). This is a lean budget with no founder salary. Without acquisition spending, 40 paying businesses cover it.

| Projection assumption | Value |
|---|---|
| Churn | 5% a month, so customers stay about 20 months |
| Cost to win a customer | about US$300, including trials (about US$10 per paying customer at one in five converting) |
| Growth | 15 new paying customers a month from month 2 |
| Sales cycle | self-serve within the trial for Starter and Growth; one to three months for Enterprise, sold by the founder (the US$300 average holds only while Enterprise stays near 5% of customers) |

Over 20 months a customer brings in about **US$1,610** of contribution, **5.4 times** the US$300 it costs to win them, earned back in under 4 months. Winning 15 a month costs US$4,500. Covering that and the fixed costs takes **96 paying businesses, reached in month 9**. Getting there needs about **US$32,500**: US$6,200 to build and pilot, and US$26,300 of operating losses.

| Month | Paying businesses | Revenue per month | Profit per month |
|---|---|---|---|
| 3 | 29 | US$2,850 | −US$5,300 |
| 6 | 68 | US$6,600 | −US$2,200 |
| 9 | 101 | US$9,800 | +US$450 |
| 12 | 129 | US$12,600 | +US$2,700 |

## 6. Risks and sensitivity

**The rule that decides it.** At 15 new customers a month, fixed costs add about US$212 to each one (US$3,180 ÷ 15). We only break even if a customer's lifetime contribution beats the cost to win them plus that US$212. In the base case it is US$1,610 against US$512.

**The assumption that matters most is the cost to win a customer.** We have no data on it, and we pay it again every month we grow. If it doubles, the cash needed nearly triples.

Each row changes one assumption at 15 new customers a month. "Cash needed" is the largest cumulative operating loss, before build costs.

| What if we're wrong | What changes | Contribution per business | Break-even | Cash needed |
|---|---|---|---|---|
| Base case | as above | US$80 | 96 businesses, month 9 | US$26,300 |
| AI costs double | AI and search about US$18 | US$72 | 108, month 10 | US$30,000 |
| Prices 30% lower | average revenue about US$68 | US$52 | 148, month 15 | US$44,000 |
| Cost to win doubles | US$600 per customer | US$80 | 152, month 15 | US$71,000 |
| Relevance below target | churn 8%; lifetime contribution about US$1,000 | US$80 | 96, month 10 | US$28,000 |
| No Enterprise customers | 75% Starter, 25% Growth | US$59 | 130, month 13 | US$38,000 |
| Rand weakens to R20 = US$1 | revenue and salary in rand worth less; AI billed in dollars | US$71 | 104, month 10 | US$28,000 |
| **All four at once** | AI ×2, prices −30%, US$600 to win, 8% churn | US$43 | **never** | unlimited |

**Why "never".** With the first four rows all wrong, a customer brings in about US$43 a month for 12.5 months: about US$540, less than the US$600 it costs to win them. Every new customer loses money, so no growth rate breaks even. At 15 a month the cost to win would have to fall below about US$330 to break even at all; at US$300 the same scenario breaks even in month 36 and needs about US$76,000. Our response: stop paid acquisition, sell through business associations and referrals, and move planning steps to smaller models until the rule holds on measured numbers.

**How far each assumption can move** (others at base) and still break even within two years:

| Assumption | Base | Limit |
|---|---|---|
| Monthly churn | 5% | about 15% |
| Cost to win a customer | US$300 | about US$900 |
| Average price | US$97 | about 45% lower |
| AI and search cost | US$8.78 | nearly 6 times higher |

**Other risks, and what limits them:**

- **Irrelevant or closed opportunities** drive churn. Every item is sourced, unsourced items are dropped, Verified appears only when the page confirms it is open, and feedback shapes the next run.
- **Willingness to pay.** No pricing evidence yet, so the pilot ends with a request to pay, not a survey.
- **Enterprise concentration.** 5% of customers supply about 29% of contribution. No hiring on Enterprise revenue until contracts are signed.
- **A cheaper or bigger rival.** Tender services could add AI matching, and general AI assistants can do a one-off search when asked. Prospect runs unasked and keeps each business's corrected profile and feedback history. That is a head start, not a moat.
- **One developer.** A second at R55,000 a month moves break-even to about 134 businesses.
- **One AI provider.** Prices or terms could change. The table above shows break-even within two years even if AI and search cost nearly six times the base, and smaller models can take the simpler steps.
- **Data protection (POPIA and GDPR).** Research reads public information about businesses, not people. We hold the account holder's email and feedback. Enterprise data stays in the customer's AWS account.

**Warning signs, set before we start.** The pilot gates are in section 4, and nothing is spent on winning customers until a pilot passes. After launch:

| Signal | We change course if | What we would do |
|---|---|---|
| Cost to win, first 30 paying customers | over US$450 | referrals only (at US$450, break-even is 124 businesses in month 12, needing about US$45,000) |
| Monthly churn, months 2–4 | above 8% | treat relevance as below target: pause acquisition and fix the search plan before spending more |

## 7. Why Prospect: value and positioning

**Positioning:** for South African owner-managers with no bid or partnerships team, Prospect is the opportunity feed that starts from your website instead of your keywords, and covers tenders, partners, funding and regulation in one place.

**What you get:** a name and a website in; 2–3 minutes later, open opportunities, each with its source, why it fits and the next step. Then a weekly email that gets closer to what you want each time you save, dismiss or steer. From R899 a month, about a quarter of the R3,900 of time it replaces.

| | Setup | Covers | Starts from your business | Checks it is open | Learns from you | Cost |
|---|---|---|---|---|---|---|
| Doing it by hand | hours every week | what you have time for | yes | you check each one | yes, in your time | about R3,900 a month of time (estimate) |
| Tender alert services | keywords, categories, regions | tenders | no | no | no, you edit keywords | subscription |
| Google Alerts, newsletters | queries, mailing lists | whatever the words match | no | no | no | free, paid in reading time |
| Sales and competitive intelligence | onboarding, target profile | leads or competitors | partly | vendor database | some | annual contracts for larger teams |
| A consultant | briefings | what you pay for | yes | yes | yes | by the day |
| **Prospect** | **name and website** | **eight focus areas** | **yes, researched** | **Verified; unsourced dropped** | **Save, Not for me, Steer** | **from R899 a month** |

**Where the alternatives are better:** a consultant can write and submit the bid, and a dedicated tender service goes deeper into tender documents. Prospect does the part nobody has time for: looking everywhere, every week, and bringing back the few things worth acting on. Thandi can keep her tender alerts: Prospect adds the tenders her keywords miss and the seven focus areas they never cover.

**Why a copy would start behind:**

- **It learns each business.** Every save, dismissal and Steer note shapes the next run. After a few months a customer's feed reflects their history, and a new tool starts from zero.
- **It checks before it shows.** Reading the page, confirming it is open, explaining the fit and dropping anything unsourced is a pipeline tuned on live runs.
- **It plans by business model, for South Africa.** Keyword tools need you to know what to look for, and global tools aren't built for procurement spread across provinces, municipalities and public entities.

**What comes next: an agent that acts.** Every card already names the action an agent would take: "Register me", "Apply for me", "Prepare my bid", "Book my stand". The next step is the agent behind that button. It drafts the registration, application or bid from the business profile, and the owner approves it before anything is sent.
