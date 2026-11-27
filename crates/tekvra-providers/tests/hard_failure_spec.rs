//! Unit spec for `ProviderError::is_hard_failure` (fallback-chain trigger classification).

use tekvra_providers::ProviderError;

#[test]
fn hard_failures_cover_accounts_transport_and_nonretryable_http() {
    // Exhausted accounts: always hard.
    assert!(
        ProviderError::NoEligibleAccount {
            provider: "openai".to_string(),
            reason: "quota".to_string(),
        }
        .is_hard_failure()
    );

    // Transport: always hard.
    assert!(
        ProviderError::Transport {
            provider: "openai".to_string(),
            message: "connection refused".to_string(),
        }
        .is_hard_failure()
    );

    // Non-retryable HTTP (e.g., 401): hard.
    assert!(
        ProviderError::HttpStatus {
            provider: "openai".to_string(),
            status: 401,
            body: "unauthorized".to_string(),
        }
        .is_hard_failure()
    );
    assert!(
        ProviderError::HttpStatus {
            provider: "openai".to_string(),
            status: 400,
            body: "bad request".to_string(),
        }
        .is_hard_failure()
    );

    // Retryable HTTP (429/502/503/504/529): NOT hard — the same-model retry path owns these.
    for status in [429, 502, 503, 504, 529] {
        assert!(
            !ProviderError::HttpStatus {
                provider: "openai".to_string(),
                status,
                body: String::new(),
            }
            .is_hard_failure(),
            "HTTP {status} is retryable"
        );
    }

    // Everything else: decode/OAuth/config issues are not chain triggers.
    assert!(
        !ProviderError::Decode {
            provider: "openai".to_string(),
            message: "bad json".to_string(),
        }
        .is_hard_failure()
    );
    assert!(
        !ProviderError::OAuth {
            message: "refresh failed".to_string(),
        }
        .is_hard_failure()
    );
    assert!(
        !ProviderError::UnknownProvider {
            provider: "nope".to_string(),
        }
        .is_hard_failure()
    );
}
