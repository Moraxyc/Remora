//! Signing in with an OpenID Connect provider: the authorization code flow with PKCE. Remora is
//! the relying party; the provider decides who may sign in, and a user is the provider's
//! (issuer, `sub`).

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::url::Url;
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope,
};

/// How long the provider's discovery document and keys are trusted before they are fetched
/// again, so that rotated keys are picked up.
const DISCOVERY_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// How long a user has to finish signing in at the provider.
const FLOW_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Bounds every request to the provider.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The path the provider sends users back to.
pub const CALLBACK_PATH: &str = "/api/web/oidc/callback";

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// The provider users sign in with.
pub struct OidcProvider {
    issuer: IssuerUrl,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    redirect: RedirectUrl,
    /// What the sign-in button calls it.
    pub name: String,
    http: reqwest::Client,
    discovered: tokio::sync::Mutex<Option<(Instant, Client)>>,
    /// Sign-ins under way, by their `state`.
    flows: Mutex<HashMap<String, Flow>>,
}

struct Flow {
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    next: String,
    started: Instant,
}

/// Who signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub name: String,
}

/// Why signing in failed. Details stay out: provider errors can echo request parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The provider could not be reached or described itself wrongly.
    Discovery,
    /// The callback names no sign-in under way, or one that expired.
    UnknownFlow,
    /// The provider refused, or answered the code exchange wrongly.
    Exchange,
    /// The ID token is missing or does not check out.
    IdToken,
}

impl Failure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery failed",
            Self::UnknownFlow => "unknown or expired sign-in",
            Self::Exchange => "code exchange failed",
            Self::IdToken => "invalid ID token",
        }
    }
}

/// How Remora is registered with the provider.
pub struct OidcRegistration {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub name: String,
}

#[derive(Debug)]
pub enum InvalidOidc {
    Issuer,
    Http(reqwest::Error),
}

impl fmt::Display for InvalidOidc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Issuer => f.write_str("oidc.issuer is not a URL"),
            Self::Http(error) => {
                write!(f, "cannot set up an HTTP client: {error}")?;
                let mut source = std::error::Error::source(error);
                while let Some(cause) = source {
                    write!(f, ": {cause}")?;
                    source = cause.source();
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for InvalidOidc {}

impl OidcProvider {
    /// The provider of `settings`, sending users back to `public_url`.
    pub fn new(settings: OidcRegistration, public_url: &str) -> Result<Self, InvalidOidc> {
        let issuer = IssuerUrl::new(settings.issuer).map_err(|_| InvalidOidc::Issuer)?;
        let redirect = RedirectUrl::new(format!("{public_url}{CALLBACK_PATH}"))
            .expect("the public URL is an origin");
        // Following redirects would let a provider point Remora at other servers.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(InvalidOidc::Http)?;
        Ok(Self {
            issuer,
            client_id: ClientId::new(settings.client_id),
            client_secret: settings.client_secret.map(ClientSecret::new),
            redirect,
            name: settings.name,
            http,
            discovered: tokio::sync::Mutex::default(),
            flows: Mutex::default(),
        })
    }

    /// Starts a sign-in that returns to `next`: the provider's authorization URL, and the
    /// `state` the browser has to come back with.
    pub async fn start(&self, next: String) -> Result<(Url, String), Failure> {
        let client = self.client().await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("profile".to_owned()))
            .add_scope(Scope::new("email".to_owned()))
            .set_pkce_challenge(challenge)
            .url();
        let mut flows = self.flows.lock().expect("flow lock poisoned");
        flows.retain(|_, flow| flow.started.elapsed() < FLOW_LIFETIME);
        flows.insert(
            state.secret().clone(),
            Flow {
                verifier,
                nonce,
                next,
                started: Instant::now(),
            },
        );
        Ok((url, state.secret().clone()))
    }

    /// Where sign-in `state` was to return to, forgetting it. `None` if there is no such
    /// sign-in under way.
    pub fn abandon(&self, state: &str) -> Option<String> {
        let mut flows = self.flows.lock().expect("flow lock poisoned");
        flows.remove(state).map(|flow| flow.next)
    }

    /// Finishes sign-in `state` with the provider's `code`: who signed in, and where to return.
    pub async fn finish(&self, state: &str, code: String) -> Result<(Identity, String), Failure> {
        let flow = {
            let mut flows = self.flows.lock().expect("flow lock poisoned");
            flows
                .remove(state)
                .filter(|flow| flow.started.elapsed() < FLOW_LIFETIME)
                .ok_or(Failure::UnknownFlow)?
        };
        let client = self.client().await?;
        let http = |request| send(self.http.clone(), request);
        let tokens = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|_| Failure::Discovery)?
            .set_pkce_verifier(flow.verifier)
            .request_async(&http)
            .await
            .map_err(|_| Failure::Exchange)?;
        let id_token = openidconnect::TokenResponse::id_token(&tokens).ok_or(Failure::IdToken)?;
        let claims = id_token
            .claims(&client.id_token_verifier(), &flow.nonce)
            .map_err(|_| Failure::IdToken)?;
        let subject = claims.subject().as_str().to_owned();
        let name = claims
            .name()
            .and_then(|name| name.get(None))
            .map(|name| name.as_str().to_owned())
            .or_else(|| {
                claims
                    .preferred_username()
                    .map(|name| name.as_str().to_owned())
            })
            .or_else(|| claims.email().map(|email| email.as_str().to_owned()))
            .unwrap_or_else(|| subject.clone());
        let identity = Identity {
            issuer: self.issuer.as_str().to_owned(),
            subject,
            name,
        };
        Ok((identity, flow.next))
    }

    /// The provider's client, discovered at most [`DISCOVERY_LIFETIME`] ago.
    async fn client(&self) -> Result<Client, Failure> {
        let mut discovered = self.discovered.lock().await;
        if let Some((at, client)) = discovered.as_ref() {
            if at.elapsed() < DISCOVERY_LIFETIME {
                return Ok(client.clone());
            }
        }
        let http = |request| send(self.http.clone(), request);
        let metadata = CoreProviderMetadata::discover_async(self.issuer.clone(), &http)
            .await
            .map_err(|_| Failure::Discovery)?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            self.client_id.clone(),
            self.client_secret.clone(),
        )
        .set_redirect_uri(self.redirect.clone());
        *discovered = Some((Instant::now(), client.clone()));
        Ok(client)
    }
}

/// Sends a request of the OpenID Connect library. The future owns its client, so that it does
/// not borrow the provider across awaits.
async fn send(http: reqwest::Client, request: HttpRequest) -> Result<HttpResponse, reqwest::Error> {
    let response = http.execute(request.try_into()?).await?;
    let mut answer = HttpResponse::new(Vec::new());
    *answer.status_mut() = response.status();
    *answer.headers_mut() = response.headers().clone();
    *answer.body_mut() = response.bytes().await?.to_vec();
    Ok(answer)
}

impl fmt::Debug for OidcProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcProvider")
            .field("issuer", &self.issuer.as_str())
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
