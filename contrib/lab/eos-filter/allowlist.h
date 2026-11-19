/* The EOS allowlist: the only requests a lab instance may send to Epic Online Services.
 *
 * This is the one place to change it (design/LAB.md section 3.1 documents it; the filter's log,
 * runs/r<N>/eos-filter.log, says what was blocked). Everything not listed here is blocked:
 * cloud saves (Player Data Storage), Title Storage, achievements, stats, leaderboards,
 * presence, friends, sessions and lobbies, user lookups, metrics and SDK telemetry, and
 * anything unrecognised. An entry only ever *allows* sign-in and ownership: never add one that
 * writes to the person's account (redeeming entitlements, creating or linking accounts,
 * unlocking, ingesting, uploading) or reports anything.
 *
 * Pattern syntax, matched against the request path without its query string, segment by
 * segment and case-sensitively:
 *   literal   that segment exactly
 *   {}        any one non-empty segment (an account id, a product id, a platform)
 *   {v}       a version segment: "v" and one or two digits (EOS SDK 1.7 says v1, the docs v2-v4)
 * Paths with percent-escapes, backslashes, empty, "." or ".." segments never match.
 *
 * Sources, given per entry:
 *   [sdk]   route strings in EOSSDK-Win64-Shipping.dll 1.7.1-13992660 (Alan Wake Remastered),
 *           under the service base URLs the SDK names ("[SDKConfigService]
 *           BaseUrl=https://api.epicgames.dev/sdk") or that its config fetch returns
 *   [auth]  dev.epicgames.com/docs/web-api-ref/authentication (Epic Account Services)
 *   [conn]  dev.epicgames.com/docs/web-api-ref/connect-web-api (EOS Connect)
 *   [ecom]  dev.epicgames.com/docs/web-api-ref/ecom-web-apis
 *   [mirror] the EOS OpenAPI mirror at apis.io (epic-games-ecom-api): the same resources
 *           without the "/epic" prefix
 */
#ifndef QUIVER_EOS_ALLOWLIST_H
#define QUIVER_EOS_ALLOWLIST_H

/* The hosts the filter applies to (lowercase). Requests to every other host pass untouched.
 * AWR's EOS SDK names only this one, for every service, sign-in included. */
static const char *const EF_FILTERED_HOSTS[] = {
    "api.epicgames.dev",
};

struct ef_allow {
    const char *method;
    const char *pattern;
    const char *why; /* what it is, as the log shows it */
};

static const struct ef_allow EF_ALLOWLIST[] = {
    /* --- SDK configuration (read only) --- */
    /* [sdk] SDKConfigService "/v1/default": the SDK's default settings for the platform. */
    { "GET", "/sdk/v1/default", "SDK config (default)" },
    /* [sdk] SDKConfigService "/v1/product/{productId}": the product's service URLs and flags.
     * Without it the SDK never initialises. */
    { "GET", "/sdk/v1/product/{}", "SDK config (product)" },

    /* --- Epic account sign-in (Epic Account Services) --- */
    /* [sdk] "/epic/oauth/v1/token", [auth] POST /epic/oauth/v2/token: exchanges the launch's
     * single-use exchange code (and later its refresh token) for the game's access token. */
    { "POST", "/epic/oauth/{v}/token", "Epic account sign-in (token)" },
    /* [sdk] "/epic/oauth/v1/tokenInfo", [auth] POST /epic/oauth/v2/tokenInfo: checks that token. */
    { "POST", "/epic/oauth/{v}/tokenInfo", "Epic account token check" },
    { "GET", "/epic/oauth/{v}/tokenInfo", "Epic account token check" },
    /* [sdk] "/epic/oauth/v1/revoke", [auth] POST /epic/oauth/v2/revoke: the SDK ending its own
     * token at logout. It ends only the game's session from this launch (never quiverd's), and
     * is better than leaving that token live until it expires. */
    { "POST", "/epic/oauth/{v}/revoke", "Epic account sign-out (revoke the lab's own token)" },
    /* [auth] GET /epic/oauth/v2/.well-known/jwks.json: Epic's public keys, to verify ID tokens. */
    { "GET", "/epic/oauth/{v}/.well-known/jwks.json", "Epic account public keys" },

    /* --- EOS Connect sign-in (product user / client credentials) --- */
    /* [sdk] "/auth/v1/oauth/token", [conn] POST /auth/v1/oauth/token: the Connect login (an
     * Epic token for the existing product user's token) and the SDK's client credentials.
     * Not "/auth/v1/users" (creates a product user) or "/auth/v1/accounts*" (links accounts,
     * makes device ids): those write to the account, so they stay blocked. */
    { "POST", "/auth/v1/oauth/token", "EOS Connect sign-in (token)" },

    /* --- Ownership and entitlements (read only; checks, never redeems) --- */
    /* [sdk] "/v1/platforms/{platform}/identities/{identityId}/ownership",
     * [ecom] GET /epic/ecom/v3/platforms/EPIC/identities/{id}/ownership?nsCatalogItemId=…:
     * does the account own the game. */
    { "GET", "/epic/ecom/{v}/platforms/{}/identities/{}/ownership", "ownership check" },
    { "GET", "/ecom/{v}/platforms/{}/identities/{}/ownership", "ownership check ([mirror] path)" },
    /* [sdk] "/v1/platforms/{platform}/identities/{identityId}/ownershipToken",
     * [ecom] POST /epic/ecom/v3/platforms/{platform}/identities/{id}/ownershipToken: a signed
     * statement of the same, for the game's own checks. */
    { "POST", "/epic/ecom/{v}/platforms/{}/identities/{}/ownershipToken", "ownership token" },
    { "POST", "/ecom/{v}/platforms/{}/identities/{}/ownershipToken", "ownership token ([mirror] path)" },
    /* [sdk] "/v1/identities/{identityId}/entitlements",
     * [ecom] GET /epic/ecom/v4/identities/{id}/entitlements: the account's entitlements (the
     * game and its DLC). Not ".../entitlements/redeem" (POST: marks them redeemed). */
    { "GET", "/epic/ecom/{v}/identities/{}/entitlements", "entitlements (read)" },
    { "GET", "/ecom/{v}/identities/{}/entitlements", "entitlements (read, [mirror] path)" },
    /* [ecom] POST /epic/ecom/v4/platforms/{platform}/identities/{id}/entitlementToken: a signed
     * statement of the entitlements (newer SDKs). */
    { "POST", "/epic/ecom/{v}/platforms/{}/identities/{}/entitlementToken", "entitlement token" },
};

#define EF_ALLOWLIST_LEN (sizeof EF_ALLOWLIST / sizeof EF_ALLOWLIST[0])
#define EF_FILTERED_HOSTS_LEN (sizeof EF_FILTERED_HOSTS / sizeof EF_FILTERED_HOSTS[0])

#endif
