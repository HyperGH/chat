use super::auth::{generate_hash, validate_credentials};
use super::rejections::handle_rejection;
use crate::dispatch;
use crate::models::appstate::APP;
use crate::models::auth::{Credentials, StoredCredentials, Token};
use crate::models::channel::{Channel, ChannelLike};
use crate::models::guild::Guild;
use crate::models::member::{Member, UserLike};
use crate::models::rejections::{BadRequest, Forbidden, InternalServerError, Unauthorized};
use crate::models::rest::{CreateChannel, CreateGuild, CreateMessage, CreateUser};
use crate::models::snowflake::Snowflake;
use crate::models::user::User;
use crate::models::{gateway_event::GatewayEvent, message::Message};
use governor::clock::{QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use nonzero_ext::nonzero;
use secrecy::ExposeSecret;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use warp::filters::BoxedFilter;
use warp::http::{header, Method};
use warp::Filter;

type SharedIDLimiter =
    Arc<RateLimiter<u64, DashMapStateStore<u64>, QuantaClock, NoOpMiddleware<QuantaInstant>>>;

/// A filter that checks for and validates a token.
pub fn needs_token() -> BoxedFilter<(Token,)> {
    let token_auth = warp::header("authorization").and_then(validate_token);
    token_auth.boxed()
}

/// A filter that checks for and validates a token, and enforces a rate limit.
pub fn needs_limit(id_limiter: SharedIDLimiter) -> BoxedFilter<(Token,)> {
    let limit = needs_token()
        .and(warp::any())
        .map(move |t| (t, id_limiter.clone()))
        .untuple_one()
        .and_then(validate_limit);
    limit.boxed()
}

pub fn get_routes() -> BoxedFilter<(impl warp::Reply,)> {
    // https://javascript.info/fetch-crossorigin
    // https://developer.mozilla.org/en-US/docs/Web/HTTP/CORS
    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec![
            Method::GET,
            Method::POST,
            Method::DELETE,
            Method::OPTIONS,
            Method::PUT,
            Method::PATCH,
        ])
        .allow_headers(vec![
            header::CONTENT_TYPE,
            header::ORIGIN,
            header::AUTHORIZATION,
            header::CACHE_CONTROL,
        ])
        .max_age(Duration::from_secs(3600));

    let message_create_lim: SharedIDLimiter = Arc::new(RateLimiter::keyed(
        Quota::per_second(nonzero!(5u32)).allow_burst(nonzero!(5u32)),
    ));

    let create_user = warp::path!("users")
        .and(warp::post())
        .and(warp::body::content_length_limit(1024 * 16))
        .and(warp::body::json())
        .and_then(user_create);

    let login = warp::path!("users" / "auth")
        .and(warp::post())
        .and(warp::body::content_length_limit(1024 * 16))
        .and(warp::body::json())
        .and_then(user_auth);

    let query_self = warp::path!("users" / "@self")
        .and(needs_token())
        .and(warp::get())
        .and_then(user_getself);

    let create_msg = warp::path!("channels" / Snowflake / "messages")
        .and(needs_limit(message_create_lim))
        .and(warp::post())
        .and(warp::body::content_length_limit(1024 * 16))
        .and(warp::body::json())
        .and_then(create_message);

    let create_channel = warp::path!("guilds" / Snowflake / "channels")
        .and(needs_token())
        .and(warp::post())
        .and(warp::body::content_length_limit(1024 * 16))
        .and(warp::body::json())
        .and_then(create_channel);

    let fetch_channel = warp::path!("channels" / Snowflake)
        .and(needs_token())
        .and(warp::get())
        .and_then(fetch_channel);

    let create_guild = warp::path!("guilds")
        .and(needs_token())
        .and(warp::post())
        .and(warp::body::content_length_limit(1024 * 16))
        .and(warp::body::json())
        .and_then(create_guild);

    let fetch_guild = warp::path!("guilds" / Snowflake)
        .and(needs_token())
        .and(warp::get())
        .and_then(fetch_guild);

    let fetch_self_guilds = warp::path!("users" / "@self" / "guilds")
        .and(needs_token())
        .and(warp::get())
        .and_then(fetch_self_guilds);

    let fetch_member = warp::path!("guilds" / Snowflake / "members" / Snowflake)
        .and(needs_token())
        .and(warp::get())
        .and_then(fetch_member);

    let fetch_member_self = warp::path!("guilds" / Snowflake / "members" / "@self")
        .and(needs_token())
        .and(warp::get())
        .and_then(fetch_member_self);

    create_msg
        .or(create_user)
        .or(login)
        .or(query_self)
        .or(create_channel)
        .or(fetch_channel)
        .or(create_guild)
        .or(fetch_guild)
        .or(fetch_member)
        .or(fetch_member_self)
        .or(fetch_self_guilds)
        .recover(handle_rejection)
        .with(cors)
        .boxed()
}

// Note: Needs to be async for the `and_then` combinator
/// Validate a token and return the parsed token data if successful.
async fn validate_token(token: String) -> Result<Token, warp::Rejection> {
    Token::validate(&token, "among us").await.map_err(|_| {
        warp::reject::custom(Unauthorized {
            message: "Invalid or expired token".into(),
        })
    })
}

// Check the limiter with the key being the token's user_id
async fn validate_limit(token: Token, limiter: SharedIDLimiter) -> Result<Token, warp::Rejection> {
    let user_id = token.data().user_id();
    limiter.check_key(&user_id.into()).map_err(|e| {
        warp::reject::custom(BadRequest {
            message: format!(
                "Rate limit exceeded, try again at: {:?}",
                e.earliest_possible()
            ),
        })
    })?;
    Ok(token)
}

/// Add a new ID-based ratelimiter to the filter.
///
/// ## Arguments
///
/// * `limiter` - The ratelimiter to add
pub fn with_id_limiter(
    limiter: SharedIDLimiter,
) -> impl Filter<Extract = (SharedIDLimiter,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || limiter.clone())
}

/// Send a new message and return the message data.
///
/// ## Arguments
///
/// * `token` - The authorization token
/// * `payload` - The CreateMessage payload
///
/// ## Returns
///
/// * [`Message`] - A JSON response containing a [`Message`] object
///
/// ## Endpoint
///
/// POST `/channels/{channel_id}/messages`
async fn create_message(
    channel_id: Snowflake,
    token: Token,
    payload: CreateMessage,
) -> Result<impl warp::Reply, warp::Rejection> {
    let channel = Channel::fetch(channel_id).await.ok_or_else(|| {
        tracing::error!("Failed to fetch channel from database");
        warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        })
    })?;

    let member = Member::fetch(token.data().user_id(), channel.guild_id())
        .await
        .ok_or_else(|| {
            tracing::error!("Failed to fetch member from database");
            warp::reject::custom(InternalServerError {
                message: "A database transaction error occured.".into(),
            })
        })?;

    let message = Message::from_payload(UserLike::Member(member), channel_id, payload).await;

    if let Err(e) = message.commit().await {
        tracing::error!("Failed to commit message to database: {}", e);
        return Err(warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        }));
    }

    dispatch!(GatewayEvent::MessageCreate(message.clone()));
    Ok(warp::reply::with_status(
        warp::reply::json(&message),
        warp::http::StatusCode::CREATED,
    ))
}

/// Create a new user and return the user data and token.
///
/// ## Arguments
///
/// * `payload` - The CreateUser payload, containing the username and password
///
/// ## Returns
///
/// * [`User`] - A JSON response containing the created [`User`] object
///
/// ## Endpoint
///
/// POST `/users`
async fn user_create(payload: CreateUser) -> Result<impl warp::Reply, warp::Rejection> {
    let password = payload.password.clone();

    let user = match User::from_payload(payload).await {
        Ok(user) => user,
        Err(e) => {
            tracing::debug!("Invalid user payload: {}", e);
            return Err(warp::reject::custom(BadRequest {
                message: e.to_string(),
            }));
        }
    };

    if User::fetch_by_username(user.username()).await.is_some() {
        tracing::debug!("User with username {} already exists", user.username());
        return Err(warp::reject::custom(BadRequest {
            message: format!("User with username {} already exists", user.username()),
        }));
    }

    let credentials = StoredCredentials::new(
        user.id(),
        generate_hash(&password).expect("Failed to generate password hash"),
    );

    // User needs to be committed before credentials to avoid foreign key constraint
    if let Err(e) = user.commit().await {
        tracing::error!("Failed to commit user to database: {}", e);
        return Err(warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        }));
    } else if let Err(e) = credentials.commit().await {
        tracing::error!("Failed to commit credentials to database: {}", e);
        return Err(warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        }));
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&user),
        warp::http::StatusCode::CREATED,
    ))
}

/// Validate a user's credentials and return a token if successful.
///
/// ## Arguments
///
/// * `credentials` - The user's credentials
///
/// ## Returns
///
/// * `{"user_id": user_id, "token": token}` - A JSON response containing the session token and user_id
///
/// ## Endpoint
///
/// POST `/users/auth`
async fn user_auth(credentials: Credentials) -> Result<impl warp::Reply, warp::Rejection> {
    let user_id = match validate_credentials(credentials).await {
        Ok(user_id) => user_id,
        Err(e) => {
            tracing::debug!("Failed to validate credentials: {}", e);
            return Err(warp::reject::custom(Unauthorized {
                message: "Invalid credentials".into(),
            }));
        }
    };

    let Ok(token) = Token::new_for(user_id, "among us") else {
        tracing::error!("Failed to create token for user: {}", user_id);
        return Err(warp::reject::custom(InternalServerError { message: "Failed to generate session token.".into() }));
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&json!({"user_id": user_id, "token": token.expose_secret()})),
        warp::http::StatusCode::OK,
    ))
}

/// Get the current user's data.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
///
/// ## Returns
///
/// * [`User`] - A JSON response containing the user's data
///
/// ## Endpoint
///
/// GET `/users/@self`
async fn user_getself(token: Token) -> Result<impl warp::Reply, warp::Rejection> {
    let user = User::fetch(token.data().user_id()).await.ok_or_else(|| {
        tracing::error!("Failed to fetch user from database");
        warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        })
    })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&user),
        warp::http::StatusCode::OK,
    ))
}

/// Create a new guild and return the guild data.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
/// * `payload` - The [`CreateGuild`] payload, containing the guild name
///
/// ## Returns
///
/// * [`Guild`] - A JSON response containing the created [`Guild`] object
///
/// ## Endpoint
///
/// POST `/guilds`
async fn create_guild(
    token: Token,
    payload: CreateGuild,
) -> Result<impl warp::Reply, warp::Rejection> {
    let guild = Guild::from_payload(payload, token.data().user_id()).await;

    if let Err(e) = guild.commit().await {
        tracing::error!("Failed to commit guild to database: {}", e);
        return Err(warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        }));
    }

    if let Err(e) = guild.add_member(token.data().user_id()).await {
        tracing::error!("Failed to add guild owner to guild: {}", e);
        return Err(warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        }));
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&guild),
        warp::http::StatusCode::CREATED,
    ))
}

/// Create a new channel in a guild and return the channel data.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
/// * `guild_id` - The ID of the guild to create the channel in
/// * `payload` - The [`CreateChannel`] payload, containing the channel name
///
/// ## Returns
///
/// * [`Channel`] - A JSON response containing the created [`Channel`] object
///
/// ## Endpoint
///
/// POST `/guilds/{guild_id}/channels`
async fn create_channel(
    guild_id: Snowflake,
    token: Token,
    payload: CreateChannel,
) -> Result<impl warp::Reply, warp::Rejection> {
    let user = User::fetch(token.data().user_id()).await.ok_or_else(|| {
        tracing::error!(message = "Failed to fetch user from database", user = %token.data().user_id());
        warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        })
    })?;

    let guild = Guild::fetch(guild_id).await.ok_or_else(|| {
        warp::reject::custom(BadRequest {
            message: "The requested guild does not exist.".into(),
        })
    })?;

    if guild.owner_id() != user.id() {
        return Err(warp::reject::custom(Forbidden {
            message: "You are not the owner of this guild.".into(),
        }));
    }

    let channel = Channel::from_payload(payload, guild_id).await;

    if let Err(e) = channel.commit().await {
        tracing::error!("Failed to commit channel to database: {}", e);
        return Err(warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        }));
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&channel),
        warp::http::StatusCode::CREATED,
    ))
}

/// Fetch a guild's data.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
/// * `guild_id` - The ID of the guild to fetch
///
/// ## Returns
///
/// * [`Guild`] - A JSON response containing the fetched [`Guild`] object
///
/// ## Endpoint
///
/// GET `/guilds/{guild_id}`
async fn fetch_guild(
    guild_id: Snowflake,
    token: Token,
) -> Result<impl warp::Reply, warp::Rejection> {
    Member::fetch(token.data().user_id(), guild_id)
        .await
        .ok_or_else(|| {
            warp::reject::custom(Forbidden {
                message: "Not permitted to view resource.".into(),
            })
        })?;

    let guild = Guild::fetch(guild_id).await.ok_or_else(|| {
        tracing::error!("Failed to fetch guild from database");
        warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        })
    })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&guild),
        warp::http::StatusCode::OK,
    ))
}

/// Fetch a channel's data.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
/// * `channel_id` - The ID of the channel to fetch
///
/// ## Returns
///
/// * [`Channel`] - A JSON response containing the fetched [`Channel`] object
///
/// ## Endpoint
///
/// GET `/channels/{channel_id}`
async fn fetch_channel(
    channel_id: Snowflake,
    token: Token,
) -> Result<impl warp::Reply, warp::Rejection> {
    let channel = Channel::fetch(channel_id).await.ok_or_else(|| {
        warp::reject::custom(BadRequest {
            message: "Channel does not exist or is not available.".into(),
        })
    })?;

    // Check if the user is in the channel's guild
    Member::fetch(token.data().user_id(), channel.guild_id())
        .await
        .ok_or_else(|| {
            warp::reject::custom(Forbidden {
                message: "Not permitted to view resource.".into(),
            })
        })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&channel),
        warp::http::StatusCode::OK,
    ))
}

/// Fetch a member's data.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
/// * `guild_id` - The ID of the guild the member is in
///
/// ## Returns
///
/// * [`Member`] - A JSON response containing the fetched [`Member`] object
///
/// ## Endpoint
///
/// GET `/guilds/{guild_id}/members/{member_id}`
async fn fetch_member(
    guild_id: Snowflake,
    member_id: Snowflake,
    token: Token,
) -> Result<impl warp::Reply, warp::Rejection> {
    let member = Member::fetch(member_id, guild_id).await.ok_or_else(|| {
        warp::reject::custom(BadRequest {
            message: "Member does not exist or is not available.".into(),
        })
    })?;

    // Check if the user is in the channel's guild
    Member::fetch(token.data().user_id(), guild_id)
        .await
        .ok_or_else(|| {
            warp::reject::custom(Forbidden {
                message: "Not permitted to view resource.".into(),
            })
        })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&member),
        warp::http::StatusCode::OK,
    ))
}

/// Fetch the current user's member data.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
/// * `guild_id` - The ID of the guild the member is in
///
/// ## Returns
///
/// * [`Member`] - A JSON response containing the fetched [`Member`] object
///
/// ## Endpoint
///
/// GET `/guilds/{guild_id}/members/@self`
async fn fetch_member_self(
    guild_id: Snowflake,
    token: Token,
) -> Result<impl warp::Reply, warp::Rejection> {
    let member = Member::fetch(token.data().user_id(), guild_id)
        .await
        .ok_or_else(|| {
            warp::reject::custom(BadRequest {
                message: "Member does not exist or is not available.".into(),
            })
        })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&member),
        warp::http::StatusCode::OK,
    ))
}

/// Fetch a user's guilds.
///
/// ## Arguments
///
/// * `token` - The user's session token, already validated
///
/// ## Returns
///
/// * [`Vec<Guild>`] - A JSON response containing the fetched [`Guild`] objects
///
/// ## Endpoint
///
/// GET `/users/@self/guilds`
async fn fetch_self_guilds(token: Token) -> Result<impl warp::Reply, warp::Rejection> {
    let guilds = Guild::fetch_all_for_user(token.data().user_id()).await.map_err(|e| {
        tracing::error!(message = "Failed to fetch user guilds from database", user = %token.data().user_id(), error = %e);
        warp::reject::custom(InternalServerError {
            message: "A database transaction error occured.".into(),
        })
    })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&guilds),
        warp::http::StatusCode::OK,
    ))
}
