# Spec Delta

## ADDED Requirements

### Requirement: End-to-end CLI OAuth loopback exchange, RFC 8628 device-code polling, and token refresh
The system SHALL execute complete OAuth sign-in flows in `tekvra auth login`—awaiting the local PKCE HTTP callback and exchanging the authorization code, or requesting a device code and polling the token endpoint across `authorization_pending` and `slow_down` responses until completion—and SHALL automatically refresh expired stored OAuth credentials when a `refresh_token` is available.

#### Scenario: Loopback PKCE OAuth login completes end to end
- **WHEN** a user runs `tekvra auth login` in browser loopback mode and the OAuth authorization server redirects to the local callback with matching `state` and `code`
- **THEN** `tekvra` exchanges the `code` and PKCE `code_verifier` at the token endpoint, extracts JWT account metadata, and persists the `0600` credential record

#### Scenario: Device-code OAuth login polls through `authorization_pending`
- **WHEN** a user runs `tekvra auth login --device-code` and the token endpoint returns `authorization_pending` before issuing tokens
- **THEN** `tekvra` displays the `verification_uri` and `user_code`, polls at the server-specified interval (widening on `slow_down`), and stores the issued token upon approval

#### Scenario: Expired stored OAuth token refreshes automatically
- **WHEN** `tekvra` loads a stored OAuth credential whose `expires_at_epoch_secs` has elapsed and a `refresh_token` is present
- **THEN** the client exchanges `grant_type=refresh_token` at the provider token endpoint, updates `credentials.json`, and supplies the refreshed access token to the `AccountPool`
