# X (Twitter) connector for tapfs

## Why this was hard

The user has tried to wire X up before and bounced off. Based on the May 2026
state of the X API, the four most likely reasons:

1. **There is no Free tier anymore.** Killed Feb 6, 2026. A first-time
   developer hitting `tap mount x` against a brand-new account will get an
   immediate paywall, not a "1,500 free reads/mo" friendly start. Every test
   call costs real money.
2. **Auth doesn't fit tapfs's current shape.** v2 user-context endpoints want
   OAuth 2.0 Authorization Code with PKCE — a web browser flow with a 2-hour
   access token and a refresh-token loop. tapfs's `oauth2` spec block today
   only supports GitHub-style device flow.
3. **Bare endpoint paths return useless payloads.** `GET /2/tweets/:id`
   returns three fields by default. Without `expansions=author_id,...` and
   `tweet.fields=created_at,public_metrics,...`, the response can't be
   rendered into a meaningful markdown file. The other connectors don't have
   this problem.
4. **Free-tier OAuth scope drift through 2025** stripped likes / follows from
   the cheap tier and people built integrations that silently 403'd.

This proposal addresses all four.

---

## What pay-per-use means for the design

Pricing as of April 20, 2026 (from the dev community announcement):

| Operation | Cost |
|---|---|
| Owned read (your own posts, bookmarks, followers, lists) | **$0.001** / resource |
| Other-user read | **$0.005** / post |
| User profile read | $0.010 / user |
| `POST /tweets` (standard) | **$0.015** / request |
| `POST /tweets` containing a URL | **$0.20** / request |

There is also a soft 2M reads/mo cap above which Enterprise becomes required,
and writes carry both a per-15-min user limit and a 10k / 24h app limit.

Three implications for the connector shape:

- **Draft-first is a financial feature, not just a safety one.** Every
  accidental flush is dollars. tapfs's existing draft state machine
  (`_draft: true` / `_id: __creating__` / `_version`) gives us exactly the
  right primitive — we just need the connector to make it obvious in
  `AGENTS.md` that `mkdir + write` is free and only removing `_draft: true`
  spends.
- **The mount layout should reflect the cost gradient.** `me/` (owned reads,
  $0.001) is one branch; `users/{username}/` (other-user reads, $0.005) is
  another. An agent that's told "prefer me/ when possible" can avoid 5×
  cost on common queries.
- **URL-bearing posts ($0.20) need explicit confirmation.** Today the
  connector unconditionally flushes. We should add a connector-level
  `pre_flush_guards` mechanism (small spec extension) that lets the YAML
  declare "if body matches /https?:\/\// require `_confirm_url_cost: true`
  in frontmatter." Cheap to implement, prevents a class of $0.20 mistakes.

---

## Spec gaps to close before v1 ships

Three are mandatory, two are nice-to-have.

**Mandatory:**

1. **OAuth 2.0 Authorization Code + PKCE** as a new `auth.type` flavor.
   Browser-popped local listener (loopback redirect URI), code verifier in
   memory, refresh-token loop driven by the background service. The existing
   `device_code_url` plumbing doesn't apply — X doesn't expose a device-code
   endpoint for v2. **Without this, the connector can't write.**

2. **Per-collection default query params.** Stop encoding `expansions=` and
   `tweet.fields=` inline in every `list_endpoint` / `get_endpoint`. Add:

   ```yaml
   default_query:
     expansions: "author_id,attachments.media_keys,referenced_tweets.id"
     tweet.fields: "created_at,public_metrics,entities,referenced_tweets,..."
     user.fields: "username,name,verified,public_metrics"
   ```

   Merged into every request URL by `RestConnector::build_url`. Other
   connectors will benefit too (Notion's `?filter_properties=`, Linear's
   GraphQL field selection).

3. **`list_root` already exists** but X nests differently: the array is at
   `data` and the expansion payload lives at `includes`. The renderer needs
   to resolve `expansions` references against `includes` when building the
   markdown body. This is a render-layer extension, not a spec extension —
   add it to `RenderSpec` as `resolve_includes: true`.

**Nice-to-have:**

4. **Per-endpoint rate-limit hints.** Today `capabilities.rate_limit` is
   global. X is per-endpoint per 15-min window. Add an optional
   `rate_limit:` block at the collection level so the daemon can throttle
   intelligently and AGENTS.md can warn the agent ("DMs are 15 / 15min;
   batch your reads").

5. **`cost_hint:` per collection** — purely advisory metadata rendered into
   AGENTS.md so the agent can reason about which directory to navigate
   first. e.g. `cost_hint: "$0.001 per resource (owned)"`.

---

## Mount layout (v1)

```
/tap/x/
  index.md                          # GET /users/me — your profile
  AGENTS.md                         # generated; teaches the layout + costs

  me/                               # owned reads — cheapest tier
    posts/                          # GET /users/{me_id}/tweets
      {tweet_id}/
        index.md                    # the tweet body + author + metrics
        comments.md                 # aggregate replies (in reply_to chain)
        likes.md                    # aggregate liking users (list)
    mentions/                       # GET /users/{me_id}/mentions
    bookmarks/                      # GET /users/{me_id}/bookmarks
      {tweet_id}/index.md           # rm -rf to unbookmark (DELETE)
    lists/                          # GET /users/{me_id}/owned_lists
      {list_id}/
        index.md                    # list metadata
        members.md                  # aggregate
        tweets.md                   # aggregate (list timeline)
    following/                      # mkdir {username} = POST /following
    followers/
    dms/                            # 15/15min — handle with care
      {conversation_id}/
        thread.md                   # aggregate dm events; append = send

  users/                            # other-user reads — $0.005 tier
    {username}/                     # slug = username, resolves to id
      index.md                      # GET /users/by/username/:username
      posts/                        # GET /users/:id/tweets
      following/
      followers/

  search/
    recent/
      {query-slug}/                 # 7-day search, $0.005/result
        index.md                    # aggregate the result set
    all/                            # full archive — Pro/Enterprise only
      {query-slug}/index.md         # hidden behind capability flag

  spaces/
    {space_id}/index.md             # read-only

  communities/
    {community_id}/
      index.md
      posts/                        # write via POST /tweets w/ community_id
```

### Why this layout

- **Cost-first split.** `me/` is "owned" pricing; `users/` is "other-user."
  An agent told to prefer `me/` saves 5× on common reads.
- **Tweets are directories, not files.** A tweet has comments, likes, quote
  tweets, retweeters — modeled as subcollections under the tweet's own
  `ResourceDir`, the same way tapfs already does GitHub issues. `cat
  index.md` is the tweet; `cat comments.md` is the thread.
- **DMs as aggregate `thread.md`.** Mirrors the GitHub `comments.md`
  pattern: `cat` shows the conversation; `echo "reply" >> thread.md` posts.
  Idempotency key from `_idempotency_key` in frontmatter prevents double-
  send on retry.
- **No streaming, no full-archive in v1.** Pro tier gates them and most
  users won't have it. Add `capabilities.streaming: false /
  search_all: false` and surface in AGENTS.md.

---

## Draft semantics for `POST /tweets`

`mkdir /tap/x/me/posts/draft-launch-announcement/` seeds:

```yaml
---
_draft: true
_id:
_version:
_idempotency_key:           # auto-filled with UUID on first save
text: |

reply.in_reply_to_tweet_id:  # optional
quote_tweet_id:              # optional
community_id:                # optional
media.media_ids: []          # populated by separate media upload flow
poll:                        # optional
  duration_minutes:
  options: []
reply_settings:              # "everyone", "mentionedUsers", "followers"
---
```

Removing `_draft: true` + save → POST. On success: real `_id` overwrites
`__creating__` sentinel; `_version: 1`; the file slug is renamed to the
returned tweet id.

**URL guard** (the $0.20 thing): connector spec carries
`pre_flush_guards: [{ pattern: "https?://", require_frontmatter: { _confirm_url_cost: true } }]`.
If the body contains a URL and the frontmatter doesn't have
`_confirm_url_cost: true`, flush fails with a clear error: *"This post
contains a URL. URL-bearing posts cost $0.20 (vs. $0.015 standard). Add
`_confirm_url_cost: true` to confirm."* No silent surprise on the bill.

---

## What ships in v1 vs. later

**v1 — works with a manually-pasted Bearer token (`X_BEARER_TOKEN` env or
keychain entry):**

- `me/posts`, `me/mentions`, `me/bookmarks`, `me/lists`, `users/{username}/posts`
- `search/recent/{query}`
- `spaces/{id}` (read)
- `communities/{id}` (read + post-with-community_id)
- Like / retweet / bookmark / unfollow via `operations_spec` frontmatter
  triggers (`state: liked` patterns)
- Aggregate `comments.md`, `likes.md`, `retweeters.md`, `quote_tweets.md`
  under each tweet
- URL-cost guard
- AGENTS.md spells out the pricing model up front so the agent stops
  before doing dumb things

**v2 — requires spec changes:**

- OAuth 2.0 PKCE flow (replace manual token paste)
- Refresh-token loop in the service
- DMs (need user-context + 15/15min rate limiter)
- Media upload (chunked v2 `/2/media/upload/{initialize|append|finalize}`,
  needs binary write path — tapfs is markdown-first today)

**v3 — paid-tier features:**

- Full-archive search (`search/all/`)
- Filtered stream as a special "tail" file (`tail -f stream.md`)

---

## Testing strategy

Following the project TDD rule: **don't merge the YAML alone**. Every
behavioral change needs a failing test first.

- **Unit tests** (`src/connector/spec.rs`): the X spec parses, `default_query`
  merges into URLs as expected, `pre_flush_guards` rejects URL-bearing posts
  without confirmation.
- **Connector integration tests** under `tests/connectors/x/` against
  recorded fixtures — `httpmock` or a static JSON capture from the official
  Postman workspace. **No live API calls in CI; they cost money.**
- **Use-case row** in `tests/use-cases/` for each shipped workflow: "draft
  + post a tweet," "reply via comments.md append," "rm -rf to unbookmark,"
  "like via frontmatter state change."
- **Manual smoke test plan** documented in this proposal — runs against the
  developer's own account with the cheapest possible read (one owned
  `index.md`). Total cost: $0.001.

---

## Open questions

- **PKCE flow UX.** Browser popup vs. printed URL + paste-back-the-code?
  GitHub's device flow is print-the-code; X requires loopback redirect.
  Probably print `http://localhost:53682/callback` style + open the auth URL
  in `$BROWSER`. Need to confirm what the macOS / Linux split looks like.
- **Refresh-token storage.** Existing keychain entry stores a single value;
  we'd need to store `{access_token, refresh_token, expires_at}` as JSON.
  Either change the keychain schema (touches every other OAuth2 connector
  even though they're device-flow) or introduce a separate
  `oauth_session.yaml` per connector under `~/.tapfs/`. The latter is
  cleaner.
- **Should `users/{username}/posts/` cache aggressively?** Other-user reads
  are $0.005, so a 1h TTL is dollars saved. But X users post frequently —
  staleness matters. Connector-level `cache_ttl_seconds: 300` per collection
  seems right; thread carefully.
- **Communities write.** `POST /tweets` with `community_id` is the only
  path. Does that go under `/tap/x/me/posts/` with a `community_id`
  frontmatter field, or under `/tap/x/communities/{id}/posts/`? Probably
  both — the same draft, mounted in two places — but that's a VFS-level
  decision the current code doesn't support.

---

## Recommendation

Ship v1 against the pay-per-use tier with manual Bearer token, with the URL
guard as the headline novel feature. It's the smallest credible product:
agents can read their timeline, draft tweets safely, post them, like /
bookmark / retweet via frontmatter, and not lose a paycheck doing it. The
cost transparency in AGENTS.md is the differentiator vs. wiring up the X
SDK by hand — the agent *can reason about cost* because it's spelled out in
the same file it reads to learn the layout.

Then add PKCE and DMs in v2.
