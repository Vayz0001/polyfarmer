# Changelog

All notable changes to Polyfarmer are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). While Polyfarmer is in beta (0.x),
anything may change between releases.

## [Unreleased]

### Fixed

- **Start refuses an order too small for Polymarket.** It now checks each side's shares, at the price the order would
  rest at, against the market's minimum order (5 shares on most markets) and says how much to enter. Before, the leg
  was saved anyway, Polymarket rejected its order three times and the bot paused it.
- **An order still being cancelled no longer counts as other people's depth.** With two legs on the same outcome,
  one leg's order that was being cancelled (sent but not yet confirmed, which can take 30 seconds or more) counted as
  depth ahead of the other leg, which could then place with less real depth in front of it than you set. It now
  counts as yours, at the price and size it was placed with, until Polymarket confirms it gone.
- **A cancelled order no longer counts as depth while the market feed still shows it.** After Polymarket confirmed a
  cancel, the bot could re-place at once while its live copy of the book still showed the old order, and count that
  order as other people's depth ahead. It now keeps treating it as yours until the feed shows it gone, for 10 seconds
  at most.
- **Shutdown waits for every order placement still in progress.** If a leg was paused, expired or removed while its
  order was being placed, shutdown didn't wait for that placement, so the order could land after the final cleanup and
  stay on the book after the bot stopped. Shutdown now waits for it (up to 5 seconds), and the order it returns is
  cancelled.
- **Live updates stop when you sign out.** An open page kept receiving alerts, status and the market's order book
  after you logged out, changed the password on another device, or the session expired. The streams now end within
  10 seconds, and the page goes to the login screen.
- **A paused leg whose order is still on the book says "Pausing".** It used to say "Paused: no order" while
  Polymarket hadn't yet confirmed the cancel, so a real order could be resting behind that label. It now shows
  "Pausing" until the order is gone. The "Paused" hint also says that Activity tells you why the bot paused a leg.

## [0.1.1] - 2026-11-27

### Added

- On a market's page, **Trades** and **Top holders** can be minimized to their title bar (the chevron next to the
  title). The choice is remembered across markets and reloads, and a minimized panel loads nothing until it is
  opened again.
- The market page's price chart shows the price and time under the pointer: a line, a dot on the price and a
  label in your local time (dates only on "All"). It also works with the keyboard: focus the chart and use ← →,
  Home and End.
- A market's page shows, below the order book, the market's **Trades** and **Top holders** from Polymarket's
  public data. Trades lists the last 10 of at least $100, $1k, $10k or $100k (buttons; either side), fetched only
  when the page opens or a button is clicked. Top holders shows the 10 biggest holders of each outcome in one
  request, with shares and their value at the current mid, each linking to the trader's Polymarket profile.
  Read-only: nothing here changes what the bot trades.
- **Defaults for new markets**: Settings has a panel to set the size and safeguards (Depth ahead, Auto-pause,
  Expires) the new-order form starts with, and the form has a **Save as my defaults** button that saves what's in
  it. Saved to `defaults.json` in the data directory; markets you already farm keep their own settings. A missing
  or unusable file falls back to the built-in values ($100, $500, 5¢, 7 days), and **Reset to built-in** removes it.
- A project website at [polyfarmer.xyz](https://polyfarmer.xyz), in `site/`: what Polyfarmer does, a simulated
  live dashboard, an interactive look at Polymarket's reward scoring, install steps and an FAQ. Static files only,
  no third-party scripts or tracking, and every animation turns off for people who prefer reduced motion.
- "Stop quoting after" has a **Custom…** option, on the new-order form and in the edit drawer: type a whole number
  of hours or days, from 1 hour to 1 year (for example 12 hours before a game starts).
- A market's page can pause, resume and remove the legs you farm there: its "You're farming this" panel has the
  same statuses and buttons as the Markets table (Pause / Resume, Edit, Remove with a confirmation, Pause both /
  Resume both), refreshes every 5 seconds and right after any change, and shows even while the bot isn't trading.
  An expired leg says **Expired** (it used to read "waiting for conditions") and has an **Extend** button that opens
  Edit on its expiry. The order form warns when starting would add a second leg on an outcome you already farm.
- The order book on a market's page marks the price in the order form: clicking a bid level, typing a price or
  loading the page outlines that level with a **YOUR BID** tag, or draws a marker line where your bid would sit when
  nobody is bidding that price yet. It follows the book as it updates and the price grouping you pick.

### Changed

- Plainer wording for what a market pays: the market page says "Rewards on · **$314** a day" (was "pool $314/day"),
  and Find markets, the market picker and the Rewards page call the column **Daily rewards** (was "Daily pool",
  "Reward pool", "Pool"), in whole dollars with thousands separators ($1,905). "Your share of today's pool" now
  reads "Your share of today's rewards".
- The Rewards page's **LP rank** card is now **Rewards rank** ("of N wallets"): PolyRewards ranks every wallet that
  has earned liquidity rewards by rewards earned, which isn't the same as a ranking of liquidity providers.
- Shorter, consistent names for the order safeguards, the same in the forms, the Markets table, alerts, the README and
  the website: **Safeguards** (was "Safety & expiry"), **Depth ahead** (was "Min depth ahead" / "Min depth" /
  "Minimum depth"), **Auto-pause** (was "Auto-pause if best bid moves" / "volatility limit") and **Expires** (was
  "Stop quoting after"). Their help text now says exactly what each does: depth counts the bids above yours up to and
  including the best bid, auto-pause triggers either way and restarts on resume, and an expired leg can be extended.
  The Markets table's auto-pause tag shows the two prices that would trigger it, and the edit drawer's labels
  match the new-order form ("Size", "Distance").

### Fixed

- **The bot now reacts to a full order book from Polymarket straight away.** After a trade Polymarket can resend
  the whole book instead of a price change; the bot updated its copy but only re-checked its orders on the next
  price change or the 30-second check, so an order whose bids ahead had just been eaten (or a leg past its
  auto-pause limit) stayed exposed meanwhile. It also starts quoting as soon as the books arrive after connecting.
- **Several dashboard tabs open no longer make pages and buttons hang.** Each open page kept one or two live-update
  connections, and over plain http a browser allows only 6 per server, so three market tabs used them all. A
  background tab now closes its live updates and reopens them, with a refresh, when you come back to it. Alerts
  sent while it was in the background then show up in its feeds and on the bell, once each. If some may be missing
  (very many arrived, or the alert log was rotated meanwhile), the page says so and links to Activity.
- **Security: many wrong passwords sent at once no longer get around the login lockout.** Each guess is now
  counted before the password is checked, so a burst gets no more tries than the same guesses one at a time. This
  also applies to the setup code and to the current password when changing it. Only 4 password checks run at once.
  Any others get "Too many attempts" until one finishes.
- **Resuming a leg while it is being removed can no longer leave an untracked order on Polymarket.** Remove now
  holds the leg's order as cancelling, so nothing new is placed until the cancel is confirmed, and deletes the leg
  only if it is still paused with no order. Otherwise it is kept, with a message to remove it again.
- **Start farming refuses a price more than 50¢ below the best bid**, with the same "Distance must be between 0
  and 50 cents." message as Edit, and creates nothing. Before, Start accepted it (for example 1¢ under a 55.7¢ best
  bid) and the leg could then not be saved in Edit.
- **When your session ends (logout in another tab, a restart, the idle timeout), the whole page now goes to the
  login page.** Before, the next refresh of a live panel put a "Sign in" card inside the status strip and the
  panels (also the order book on a market's page and the Alerts window) while the rest of the old page stayed on
  screen. First run goes to the welcome page the same way.
- **Logging out now always ends the session.** If another page or panel of the same session was still loading
  when you logged out, it saved the session again as it finished and the old login cookie worked again. Logging
  out now sticks; a request still running finishes normally but can no longer restore the session.
- **A market leg with an invalid value in `markets.json` (for example a distance of 0) is no longer dropped at
  startup.** It vanished from the dashboard, the next save deleted it and any order it had resting was no longer
  tracked. It now loads paused with an Activity alert saying what's wrong; Resume refuses until Edit fixes it.
- **Dashboard preview: `cargo run --example serve` now listens on `127.0.0.1:8081` by default**, not the bot's
  port 8080, so it can run next to a bot on the same machine (`DASHBOARD_BIND` still overrides it). Its first-run
  welcome link now shows the address it actually listens on instead of always `127.0.0.1:8080`.
- **Dashboard preview: `cargo run --example serve` no longer opens the bot's `data/` folder.** Without `DEMO=1`
  it used the real bot's wallet, password and markets.json, so a market paused, edited or removed in the preview
  changed the real bot's file. It now uses its own `data-preview/` folder; `DEMO=1` still uses `data-demo/`.
- **Dashboard: a save no longer overwrites a newer one in markets.json.** A pause, resume, edit, remove or new
  market saved from the dashboard could land just after an auto-pause (or another change) and write it out of the
  file, so after a restart an auto-paused leg quoted again or a removed leg came back. Every save now happens in
  turn with the engine's own.
- **Engine: "No balance" always pauses on the second refusal.** The second "not enough balance" only paused the leg
  if it came within two minutes of the first. When the retry was held back longer (thin depth, the two-sided
  rule), it counted as a first refusal again and the leg kept retrying without ever showing "No balance". Now the
  leg pauses whenever its next attempt is refused for funds again; any other outcome in between starts over.
- **Engine: a re-peg can't undo another cancel, and doesn't hammer Polymarket.** Moving an order first marked it as
  being cancelled, then checked the market's tick size; if that check failed it "restored" the leg, which could
  wipe out a cancel another part of the bot had started (the order stayed up while the leg could place a second
  one). The tick is now checked before anything is touched. A failed tick read also used to be retried on every
  order-book update; now the leg waits 10 seconds, and only one re-peg per leg runs at a time.
- **Engine: no order on a leg you've just paused.** A decision to place made a moment before you paused (or before
  the leg expired, hit "No balance" or was removed) could still go out, leaving an order on a stopped leg until the
  next check pulled it. Placing now re-checks the leg itself at that moment.
- **Engine: a Deposit Wallet's orders are never signed the wrong way.** At start the bot asks Polygon whether your
  Polymarket wallet is a Deposit Wallet, which decides how orders are signed. If the public Polygon endpoint failed
  at that moment, it quietly assumed "no", and a Deposit Wallet then had every order rejected for the whole
  session. Now each endpoint is tried in turn, and if none can answer, the engine start fails with a clear message
  and retries shortly instead of guessing. (A wallet that answers "not supported" is a definite no.)
- **Engine: calls to Polymarket can no longer hang forever, and stray orders are cleaned up.** The Polymarket
  library sets no time limit, so a stalled connection could freeze the heartbeat watchdog, leave a leg "placing"
  for good, or block a shutdown or Remove. Every call now has a limit (5 to 30 seconds); a timeout counts as an
  outage. A placement that times out or hits a network error may still have gone through, so the bot now checks
  that market's open orders before placing there again. Every 30 seconds it also checks each market it farms and
  **cancels any open order no leg is tracking** (from a placement that timed out, a failed cancel, or one placed by
  hand on that market), with an alert.
- **Engine: shutdown leaves no orders behind.** Stopping the bot cancelled the orders it was tracking at that
  moment, but an order still being placed could land just after and stay up once the bot had exited. A stop signal
  arriving while the bot was busy starting could also be missed, so it kept trading until the service killed it. Now
  the stop signal is caught from the moment the bot starts; on shutdown nothing new is placed, placements in flight
  get a few seconds to land, everything tracked is cancelled, and a final sweep cancels any open order left on the
  markets it farms. Each step has a time limit, so a stop finishes within the service's 60 seconds.
- **Engine: a frozen market feed is noticed.** A connection that stays open but stops delivering data left the bot
  quoting on a frozen order book indefinitely. Now, if nothing arrives for 30 seconds (Polymarket answers the
  bot's ping every 10 seconds), the bot drops the connection, cancels its orders as on any disconnect, and
  reconnects. Connecting has a 10-second limit, a server that keeps dropping the connection gets a growing delay
  instead of a tight reconnect loop, and connect/disconnect alerts are limited to one per 5 minutes while it flaps.
- **Engine: a leg is never deleted automatically any more.** Three failed placements in a row used to remove the
  leg from your markets for good, and network blips, timeouts and Polymarket being busy (5xx, rate limits) counted
  as failures, so a short outage could wipe every leg. Now outages only back off (2 seconds doubling to a minute)
  and never count, with one alert if they keep happening; three real rejections in a row **pause** the leg (saved)
  with an alert giving Polymarket's reason. Fix it in Edit and Resume.
- **Engine: an order Polymarket rejects is no longer treated as placed.** Polymarket can answer a placement with
  "success: false" and no order id. The bot took that as a placed order, so the leg showed Live on an order that
  didn't exist and then stayed stuck "cancelling" it. It's now a rejection with Polymarket's reason; a refusal for
  lack of funds sent this way gets the "No balance" handling too.
- **Engine: the startup safety check no longer skips a market it couldn't read.** At start, the bot lists and
  cancels any open orders on the markets it farms. If listing one market failed, it was only logged and the bot
  started anyway, trading beside that market's old orders. Now that counts as a failed check: what was found is
  still cancelled, and the start is retried. Reading a market's orders also stops after 50 pages instead of
  possibly looping forever.
- **Engine: a reconnect no longer forgets an order placed while it was cancelling.** After the market feed dropped,
  the bot cancelled its orders and then reset every leg, including one whose order landed during that cancel, so
  the order was forgotten and the leg could place a second one beside it. Now placement stops the moment the feed
  drops (its order books are cleared until it reconnects), and only the legs whose orders were in the cancel are
  reset. The heartbeat's cancel works the same way, and pauses placement before cancelling instead of after.
- **Engine: a re-peg's new order gets the same checks as any other.** Moving an order cancelled the old one and
  placed the new one straight away at "best bid − distance", skipping the "Depth ahead" check, Polymarket's
  two-sided rule, tick handling, retries and the "No balance" handling. Now, once the old order is confirmed gone,
  the leg goes through the normal placement path at once; if the depth ahead has gone, it waits instead of placing
  unprotected. Alerts still say "Order replaced".
- **Engine: an order can no longer end up resting on Polymarket untracked.**
  - A cancel (from the engine, or from Pause/Edit on the dashboard) now only claims the order the leg holds at that
    moment. Before, a re-peg that put up a new order in between could be overwritten, and the new order was left
    resting with nothing watching it.
  - A leg paused or expired while its order was being placed now cancels that order if it landed meanwhile, instead
    of marking the leg as having none.
  - Pause and a size/distance edit also settle an order that is still being placed.
- **Engine: a bulk cancel only frees the orders Polymarket confirms gone.** After a disconnect or a lost heartbeat,
  orders Polymarket did not confirm cancelled used to be treated as gone, so a second order could be placed beside
  them. They now stay "cancelling" and are retried every 30 seconds, with an alert; the startup check refuses to
  trade while any are still open, and shutdown reports them.
- **Engine: full order-book updates after trades are no longer ignored.** Polymarket resends a token's whole book
  as a single message (often after a trade). The engine only read the list form sent on subscribe, so its book,
  which the safeguards use, could be out of date. The engine and the market page now share one reader, and best
  bid/ask come from the price levels rather than their order in the message.
- **Depth ahead no longer counts your own orders.** When the best bid moved onto a resting order, the re-peg check
  added up the bids between the new price and the best bid, which included that order itself (and any other leg of
  yours on the same outcome). So an order could move with less of other people's money ahead of it than the "Depth
  ahead" you set. Every depth check in the engine now leaves your own resting orders out.
- **"Not enough balance" no longer retries silently forever.** Before, a leg Polymarket refused for lack of funds was
  retried on every order-book update with no alert. Now it retries once after 15 seconds (a fill that is still
  settling can lock funds for a moment), and if it is refused again within two minutes the leg is **paused** and the
  Markets table and the market page show **No balance**, with one alert saying how much the order needed. Add funds or
  lower the size, then Resume. The same applies when a re-peg's new order is refused. It never counts as a failed
  placement, so it can't auto-remove the leg.
- Order form and preview fixes:
  - In **Both sides** mode the shares box showed the whole size at this side's price. It now shows this side's
    shares (half the size), labelled with the outcome ("39 No shares"), and typing shares sets the total to match.
  - Share counts are rounded the way orders are placed (to 2 decimals) in the meter and the preview, which used to
    show unrounded or whole numbers. A size just under the reward minimum (49.6 vs 50) no longer looks like it
    meets it.
  - Starting **Both sides** when the other outcome's best bid is too low to bid the same distance below it is
    refused with a clear message, instead of creating a leg that waits forever. The preview and meter say "no
    room below its best bid".
  - "Order replaced" alerts show the size the new order was placed with, not the one from before an edit.
- Links to polyfarmer.xyz now show a preview card on X, Discord and other apps: the site announced a large image
  card but never said which image, so none appeared. It now has a 1200×630 preview image and the matching tags.
- The Rewards page's rank card retries a failed lookup after 5 minutes instead of showing "rank unavailable" for
  an hour, and a failed check no longer blanks a rank it already found: it keeps showing it with "last check failed,
  retrying". While the first lookup runs it says it can take a minute.
- The Rewards page's rank card no longer says "rank unavailable": PolyRewards' leaderboard has grown to ~27 MB and
  took over two minutes to download, past the 90-second limit. It is now fetched gzipped (~6.5 MB, a few seconds),
  unpacked with a size cap, with a 3-minute limit.
- The new-order form's size accepts cents. Typing a share count fills in shares × price (108 × 21.9¢ = $23.65),
  which the box used to refuse on **Start farming** because it only took whole dollars. Saved defaults take whole
  cents too.
- Auto-pause: resuming a leg it had paused didn't restart the limit, so if the price hadn't come all the way back the
  leg sat on "Waiting" with no order and no message. Resume now restarts the limit from the current best bid (as
  changing the limit in Edit already did), after an auto-pause or a pause of your own; "Resume both" resumes both
  legs or, if a price can't be read, neither. A leg with no order yet whose best bid moves past its limit now pauses and
  says so, instead of waiting silently, and an auto-pause raises one alert even when the timer and the price feed
  see the move at the same moment.
- Accessibility (Lighthouse 100 on every dashboard page): the payout charts, Find markets' sort menus and the
  market page's size boxes have names for screen readers, the hover tooltip stays out of the accessibility tree
  until it shows, KPI links are underlined rather than told apart by colour alone, and Overview no longer jumps as
  its panels load. The browser console no longer warns about an unrecognised `bluetooth` permission on every page.
- The market page's order book drew each depth bar from that level's own size, while the Total column next to it is a
  running total, so a row could show a small bar beside a big total. Bars now draw the running total, growing away from
  the spread, on one scale for bids and asks (the deeper side is the full width), as on polyfarmer.xyz.
- Dashboard layout: at 1280px the Markets table cut off Expires and the Pause/Edit/Remove buttons, Browse titles ran
  over the numbers next to them, and Overview cut Score to "SCO". Tables now fit, the row buttons stay pinned in
  reach on narrower screens, the app works from 1024px wide, and sticky table headers no longer slide under the
  status bar.
- The 5-second Markets refresh no longer drops keyboard focus or a table's sideways scroll. The edit drawer and the
  Remove dialog keep Tab inside, return focus where it came from, and the closed drawer can't be reached by Tab
  (its shadow no longer shows along the right edge). Remove's dialog focuses Cancel and names the action. Toasts
  moved to the bottom right, clear of the page's buttons.
- Faint text, the blue and red buttons and the light theme's green, amber and blue now read at 4.5:1 or better.
- Money is shown one way: dollars and cents everywhere on Rewards ("<$0.01" for dust, the exact amount on hover),
  the same style for Size and Min depth, "<$1" instead of "$0" for tiny book levels, and amounts are rounded rather
  than cut off.
- The order preview no longer shows negative distances ("−74¢ below best bid"); the score box says roughly what size
  reaches the reward minimum. Starting a market shows "Added to your markets". Activity keeps the time of day for
  older entries and says when a filter matches nothing. The link-error page has a paste box. One name, "Find
  markets", for the market finder; Rewards uses the Markets page's status words; every status explains itself on
  hover; Browse says "ended" for markets past their end date; big event pickers tuck away markets without rewards.
- A cancel that failed (for example a network error) could still free the leg: pausing, expiry and auto-pause
  marked it "waiting" whatever the cancel's result, and any cancel left unconfirmed for 30 seconds was reset the same
  way. The old order could then keep resting on Polymarket while the leg showed as paused, or next to a new one.
  A leg is now freed only once Polymarket confirms its old order is gone. Until then the bot retries about every
  30 seconds, places nothing new for that leg, and alerts you after three failed tries (not during an outage the
  heartbeat already reported). "Gone" now means a real "not found" or a cancelled or matched order: a timeout or
  server error while checking no longer counts, and neither do two unclear cancel replies in a row.
- A replace that was in progress when its market was paused, expired or auto-paused could still place the new order.
  It now stops and places nothing. An order a replace had to discard is now cancelled with confirmation, with an
  alert if Polymarket doesn't confirm it.
- While a leg was cancelling, a paused, expired or volatile market re-sent its "Paused" alert and another cancel on
  every book update. It now waits for the cancel in progress (a volatility trip still pauses the leg, once).
- On a market priced at or below your distance (for example a 1¢ best bid with a 2¢ distance), the bot sent a cancel
  for an order that didn't exist on every book update, with a false "Order cancelled" alert. It now just waits.
- The volatility auto-pause saved `markets.json` outside the state lock, so a save racing with it (such as removing
  a market) could bring a removed market back after a restart. It now saves under the lock, and a failed save is
  reported as an alert.
- When the edit drawer rejected a value, it reopened with the saved settings and you had to retype everything. It now
  keeps what you typed.
- Pasting a sports or esports link (for example `polymarket.com/esports/league-of-legends/worlds/<match>` or
  `polymarket.com/sports/nba/<game>`) failed with "Not a Polymarket market URL". Every polymarket.com link now
  works: game and event pages, `/event/<event>/<market>`, `/market/<market>`, language prefixes such as `/es/`, and a
  bare slug. Links to other sites, and slugs with anything other than letters, digits, `-` and `_`, are refused.
- A page with several markets (a game's winner, handicaps, totals, props…) now opens the picker instead of jumping
  to the market that shares the page's slug. Markets that pay rewards come first, the main market on top, with
  each market's daily pool; markets without rewards are dimmed at the bottom.
- "Rewards on" was shown for markets with no reward pool: Polymarket gives most markets a min size and zone
  (often 50 shares and 4.5¢) whether or not anything is paid. A market now counts as rewarded only while a reward
  program is paying, and its daily pool is shown on the market page.
- When Polymarket changed a market's tick size while Polyfarmer was running (for example from 1¢ to 0.1¢), orders
  failed with "Price … has 3 decimal places" and the market was auto-removed within milliseconds. Polyfarmer now
  follows tick-size changes live, re-reads the tick from Polymarket when its order library disagrees with the
  market, and never counts a tick change as a failure.
- After a failed order, Polyfarmer waits before trying again (2 s, then 4 s, up to a minute) instead of retrying on
  the next book update, so the three failures that auto-remove a market can no longer happen in a single burst.
  Resuming a market clears the wait. Orders that fail only because collateral is still settling are retried as before.
- Saving `markets.json` from two places at once could leave a broken file that stopped Polyfarmer from starting.
  Each save now writes its own temporary file.
- Removing a market often needed several clicks: the Markets table refreshes every few seconds, and a refresh while
  the confirm dialog was open replaced the button, so Confirm silently did nothing. Refreshes now wait while the
  dialog is open.
- The Score on Markets and Overview ignored Polymarket's two-sided rule, so after pausing or removing one side the
  other kept showing its two-sided score. It now shows what Polymarket scores: the weaker side when both are live,
  a third for one side alone (midpoint 10-90¢), or nothing outside that range. Hover a score for the breakdown.
- Polymarket's "scoring" check marks could stay stale for up to 15 seconds after an order changed.
- On Windows, closing the console window (or logging off, or shutting down) killed Polyfarmer without
  cancelling its resting orders. It now cancels them first, as Ctrl+C does.
- Error messages on the Markets page vanished within a second, because the table's next refresh redrew it. They now
  also appear as a toast that stays until you close it.

### Changed

- The market page's order book no longer marks the price you're about to bid with "NEW". It only appeared when
  someone else already bid at that exact price and it pushed that row's price out of line. Clicking a bid still
  sets your price, and the order panel still shows where your bid sits.
- The shortest expiry is now 1 hour (it was 1 minute, which the forms never offered). Anything shorter is refused
  with a clear message.
- Updated `tokio-tungstenite` (the WebSocket client for Polymarket's live feeds) from 0.24 to 0.30: faster message
  reading, and one fewer duplicate dependency.
- Updated `rand` from 0.8 to 0.10. Keys, salts and nonces still come straight from the operating system's random
  source; if it ever fails, Polyfarmer now reports an error instead of crashing.
- Updated `chacha20poly1305` (wallet encryption) from 0.10 to 0.11. Wallets saved by earlier versions open
  unchanged, and wallets saved now still open in earlier versions; tests pin both.
- Updated `argon2` (password hashing) from 0.5 to 0.6, with the same settings. Existing passwords keep working,
  and passwords set now still work in earlier versions; tests pin both.
- Removing a market now cancels its order first and only removes it once the cancel is confirmed. If the cancel
  fails, the market stays, paused, so its order is still tracked instead of being left on Polymarket unwatched.
- Polymarket only rewards both sides together while a market's midpoint is under 10¢ or over 90¢, so a bid on one
  side alone there earns nothing but can still be filled. In that range the bot now quotes a market's two sides
  together: a side waits until the other can go up too, and a side left alone (the other paused, expired, short of
  depth or failing to place) is pulled after 30 seconds, then waits a little longer each time (30 s up to 10 min)
  so a failing side can't cause a place-and-cancel loop. A market farmed on one side only stays off the book in
  that range. Markets shows why a side is waiting ("Waiting" with the reason, or "Needs both sides"), and Activity
  notes each pull. Between 10¢ and 90¢, where one side alone still earns a third, nothing changes.

- The install steps use the latest release (`git clone --branch v0.1.0`) instead of `main`, and the README
  explains how to update to a new version. CONTRIBUTING describes the release steps.

## [0.1.0] - 2026-10-29

The first public beta.

### Added

- **Trading engine** for Polymarket's V2 CLOB: keeps a resting BUY a set distance below the best bid,
  re-pegs it as the book moves, and cancels it when your minimum depth, volatility limit or expiry
  says so. Cancels every resting order on shutdown, WebSocket loss or heartbeat failure.
- **Dashboard** (axum + Askama + htmx, embedded in the binary), a dark trading-terminal style:
  Overview, Markets (pause, resume, edit in place, remove), Market view with a live order book
  that includes cumulative USD depth, a reward-weight meter and a price chart, Positions, Rewards
  (daily history and all-time), a live Activity feed, and Settings.
- **Reward scoring** that follows Polymarket's rules, including the one-sided penalty and the
  two-sided requirement outside the 10-90 cent range.
- **Wallet setup** with on-chain detection of your Polymarket wallet, support for deposit wallets,
  and a balance read straight from the chain.
- **Remote access**: a one-time setup code for first run (replaces the old loopback-only check),
  `DASHBOARD_SECURE_COOKIES` for HTTPS, a startup warning when the dashboard is reachable from the
  network, and documentation for Tailscale Serve and SSH tunnels.
- **systemd unit** (`deploy/polyfarmer.service`) with a hardened sandbox, and SIGTERM handling so a
  service stop cancels resting orders before the process exits.
- **Dependency policy**: `deny.toml` (cargo-deny) and `.cargo/audit.toml` (cargo-audit); every accepted
  advisory has a written reason and review date.
- `.env.example` as the one documented list of settings, with a test that keeps it in step with the code.
- `SECURITY.md`, `CONTRIBUTING.md` and this changelog.
- When the engine can't start, the terminal, the dashboard and the activity feed now say it is usually a network,
  VPN or regional-block problem, and the terminal logs the full error.
- A fuller README: install steps for Linux, macOS and Windows, a first-run guide, screenshots, an FAQ and
  the known limits. Also issue forms (bug, feature, question), a pull request template and
  `THIRD_PARTY_NOTICES.md` with the htmx and Geist font licenses.
- CI also builds, lints and tests on Windows and macOS, not only Linux.
- Simpler Windows install steps: three `winget` commands and a Git clone, with no downloads from the project page,
  no NASM and no environment variable to set. CI now checks the Windows build with NASM removed.
- Shorter, beginner-friendly macOS install steps that also cover Macs with Rust already installed (for example
  through Homebrew).
- A logo: a blue stem with a green leaf. The dashboard shows it in the sidebar, and browser tabs have a favicon
  and touch icon for the first time. Ready-made variants (transparent, dark blue, black) are in `docs/brand`.
- Your rank among Polymarket liquidity providers on the Rewards page, with a "Top 1%"-style tier, from the public
  leaderboard files of PolyRewards (a community site). Your wallet is looked up locally; its address is never sent.

### Changed

- The name is written "Polyfarmer" in the dashboard and documentation; the program, crate and commands
  stay lowercase (`polyfarmer`).
- `tests/chaos_monkey.mjs` refuses to run unless `CHAOS_TARGET_IS_DEMO=yes` is set, because it clicks at
  random and could act on a live instance.
- The Code of Conduct names a contact address for reports.
- A clearer Rewards page. The top row shows today so far, yesterday, the last 7 days, all time and your LP rank.
  "Today, by market" lists only the markets you farm (with their status) plus any market that paid today. The daily
  chart and table are one "Daily payouts" panel. The Overview rewards panel loads in the background.
- Overview: "Rewards today" moved next to "Resting in orders" and shows yesterday's payout underneath; the rewards
  panel adds a Yesterday row.
- Reward times on the Rewards page are shown in your own time zone (12-hour) with UTC in brackets, e.g.
  "since 7:00 AM (00:00 UTC)". Without JavaScript they stay in UTC.
- Reward history now comes from your actual payouts in Polymarket's activity feed, so it starts from your first
  payout ever, not from the day Polyfarmer was installed, and can't miss or misrecord a day.
- The demo (`examples/serve`) can run on another port with `DASHBOARD_BIND`, and shows sample reward data.

### Security

- Wallet key encrypted at rest (ChaCha20-Poly1305); key material is wiped from memory after use.
  Secret files and the data directory are created owner-only from the start and written atomically.
- Admin password stored as an argon2 hash, hashed off the async runtime; new passwords must be
  12-128 characters.
- Login and setup-code throttling per source with escalating lockouts, plus a global ceiling.
- Sessions are server-side, bounded in number, self-cleaning, expire after 12 hours idle or 7 days in
  total, and are all invalidated when the password changes. The cookie is `HttpOnly`, `SameSite=Lax`,
  and `Secure` with the `__Host-` prefix over HTTPS.
- CSRF tokens on every state-changing request, cross-site requests refused, request bodies capped at
  64 KB.
- Strict Content-Security-Policy (no inline scripts, no `eval`) and the standard hardening headers; HSTS
  over HTTPS; pages are never cached.
- Every form value is range-checked; a handler panic is contained and returns a generic error.
- The process disables core dumps and ptrace; the systemd unit drops all capabilities and restricts
  syscalls and address families.
- Updated rustls, rustls-webpki, webpki-root-certs and tokio to the first fixed releases for advisories published
  in October 2026 (TLS secrets in `Debug` output, a key-exchange downgrade, distrusted emSign intermediates, an
  `AsyncFd` unsoundness), found by the weekly dependency check.
- Updated dependencies to clear three reachable advisories and a yanked crate, and updated the vendored
  htmx to 2.0.10.

### Fixed

- Today's reward total on Overview and Rewards now shows while you farm: it is the sum of the per-market amounts,
  so it always matches the rows (Polymarket's day-total figure lags during the day). The three places that show
  today's rewards now share one cached request.
- Today's rewards now include markets you have paused or stopped farming since they paid. Before, those dropped out of
  the per-market list and the total once their live share fell to 0%.
- "Today, by market" no longer lists hundreds of markets you don't farm.
- The Linux install steps include `cmake`, which the build needs on newer distributions such as Ubuntu 26.04.
- Unbounded memory growth: a string leaked on every WebSocket error, caches that never evicted, and a
  refresh task that could stay stuck after a panic. Reading the activity log no longer loads the whole
  file.
- Login lockout that never triggered.
- The "Start farming" button being replaced by the placement preview, filter selects not keeping their
  value, an empty one-week price chart, and the balance showing 0 when it was read from the wrong wallet.

### Removed

- Three old command-line smoke-test binaries, five unused engine helpers, and a redirect for a URL
  from an unreleased earlier UI.
- The one-off script and data used to create the Hacktoberfest issues (`issues.json`,
  `scripts/seed_hacktoberfest_issues.py`). They stay in Git history.
- The daily reward snapshot file (`data/reward_history.json`) and its `REWARD_HISTORY_FILE` setting; history comes
  from Polymarket's payout records instead. An existing file is simply no longer read.

[Unreleased]: https://github.com/Vayz0001/polyfarmer/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/Vayz0001/polyfarmer/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Vayz0001/polyfarmer/releases/tag/v0.1.0
