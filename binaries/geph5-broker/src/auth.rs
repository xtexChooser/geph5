use argon2::{password_hash::Encoding, Argon2, PasswordHash, PasswordVerifier};

use cached::proc_macro::cached;
use geph5_broker_protocol::{AccountLevel, AuthError, Credential, UserInfo};

use std::{
    collections::BTreeMap,
    ops::Deref as _,
    sync::{Arc, LazyLock},
    time::Duration,
};

use moka::future::Cache;
use rand::Rng as _;
use sqlx::types::chrono::Utc;

use crate::{database::POSTGRES, log_error};

pub async fn register_secret(user_id: Option<i32>) -> anyhow::Result<String> {
    let secret = (0..23)
        .map(|_| rand::thread_rng().gen_range(0..9))
        .fold(String::new(), |a, b| format!("{a}{b}"));
    let secret = format!("9{secret}");

    let mut txn = POSTGRES.begin().await?;
    let user_id = if let Some(user_id) = user_id {
        user_id
    } else {
        let (user_id,): (i32,) =
            sqlx::query_as("insert into users (createtime) values (NOW()) returning id")
                .fetch_one(&mut *txn)
                .await?;
        user_id
    };
    let hash = secret_hash(secret.clone()).await;
    sqlx::query("insert into auth_secret (id, secret_hash) values ($1, $2)")
        .bind(user_id)
        .bind(hash)
        .execute(&mut *txn)
        .await?;
    txn.commit().await?;
    Ok(secret)
}

async fn secret_hash(secret: String) -> String {
    blocking::unblock(move || {
        let hasher = Argon2::new_with_secret(
            b"geph5-pepper",
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            argon2::Params::DEFAULT,
        )
        .unwrap();
        let mut output_material = [0u8; 16];
        hasher
            .hash_password_into(secret.as_bytes(), b"geph5-pepper", &mut output_material)
            .unwrap();
        hex::encode(output_material)
    })
    .await
}

#[cached(time = 3600, result = true, sync_writes = true)]
pub async fn validate_credential(credential: Credential) -> Result<i32, AuthError> {
    match credential {
        Credential::TestDummy => Err(AuthError::Forbidden),
        Credential::LegacyUsernamePassword { username, password } => {
            Ok(validate_username_pwd(&username, &password).await?)
        }
        Credential::Secret(s) => Ok(validate_secret(&s).await?),
    }
}

pub async fn validate_secret(secret: &str) -> Result<i32, AuthError> {
    tracing::debug!(secret, "validating secret");
    // 1. Compute the hash of the supplied secret, just like we did in register_secret.
    let hash = secret_hash(secret.to_string()).await;

    // 2. Query the DB to see if any row matches this hash.
    let res: Option<(i32,)> = sqlx::query_as("SELECT id FROM auth_secret WHERE secret_hash = $1")
        .bind(hash)
        .fetch_optional(POSTGRES.deref())
        .await
        .inspect_err(log_error)
        .map_err(|_| AuthError::RateLimited)?;

    // 3. If we find a matching user_id, great; otherwise, Forbidden.
    if let Some((user_id,)) = res {
        Ok(user_id)
    } else {
        Err(AuthError::Forbidden)
    }
}

pub async fn validate_username_pwd(username: &str, password: &str) -> Result<i32, AuthError> {
    tracing::debug!(username, "validating legacy username/password");
    let res: Option<(i32, String)> =
        sqlx::query_as("select user_id,pwdhash from auth_password where username = $1")
            .bind(username)
            .fetch_optional(POSTGRES.deref())
            .await
            .inspect_err(log_error)
            .map_err(|_| AuthError::RateLimited)?;
    let (user_id, phc_string) = if let Some(res) = res {
        res
    } else {
        return Err(AuthError::Forbidden);
    };

    let phc = PasswordHash::parse(&phc_string, Encoding::B64)
        .inspect_err(log_error)
        .map_err(|_| AuthError::Forbidden)?;

    Argon2::default()
        .verify_password(password.as_bytes(), &phc)
        .map_err(|_| AuthError::Forbidden)?;

    Ok(user_id)
}

pub async fn new_auth_token(user_id: i32) -> anyhow::Result<String> {
    let token: String = std::iter::repeat(())
        .map(|()| rand::thread_rng().sample(rand::distributions::Alphanumeric))
        .map(char::from)
        .take(30)
        .collect();

    match sqlx::query("INSERT INTO auth_tokens (token, user_id) VALUES ($1, $2)")
        .bind(&token)
        .bind(user_id)
        .execute(POSTGRES.deref())
        .await
    {
        Ok(_) => Ok(token),
        Err(e) => anyhow::bail!("database failed {e}"), // If insertion fails, return RateLimited error
    }
}

#[cached(time = 60, result = true)]
pub async fn valid_auth_token(token: String) -> anyhow::Result<Option<(i32, AccountLevel)>> {
    let user_id: Option<(i32,)> =
        sqlx::query_as("SELECT user_id FROM auth_tokens WHERE token = $1")
            .bind(token)
            .fetch_optional(POSTGRES.deref())
            .await?;
    if let Some((user_id,)) = user_id {
        let expiry = get_subscription_expiry(user_id).await?;
        tracing::trace!(user_id, expiry = debug(expiry), "valid auth token");
        record_auth(user_id).await?;
        if expiry.is_none() {
            Ok(Some((user_id, AccountLevel::Free)))
        } else {
            Ok(Some((user_id, AccountLevel::Plus)))
        }
    } else {
        Ok(None)
    }
}

#[cached(time = 60, result = true)]
pub async fn get_user_info(user_id: i32) -> Result<Option<UserInfo>, AuthError> {
    let plus_expires_unix = get_subscription_expiry(user_id)
        .await
        .map_err(|_| AuthError::RateLimited)?
        .map(|u| u as u64);

    Ok(Some(UserInfo {
        user_id: user_id as _,
        plus_expires_unix,
    }))
}

pub async fn get_subscription_expiry(user_id: i32) -> anyhow::Result<Option<i64>> {
    static ALL_SUBSCRIPTIONS_CACHE: LazyLock<Cache<(), Arc<BTreeMap<i32, i64>>>> =
        LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(30))
                .build()
        });

    let all_subscriptions = ALL_SUBSCRIPTIONS_CACHE
        .try_get_with((), async {
            let all_subscriptions: Vec<(i32, i64)> = sqlx::query_as(
            "SELECT id,EXTRACT(EPOCH FROM expires)::bigint AS unix_timestamp FROM subscriptions",
        )
        .fetch_all(POSTGRES.deref())
        .await?;
            anyhow::Ok(Arc::new(all_subscriptions.into_iter().collect()))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    Ok(all_subscriptions.get(&user_id).cloned())
}

pub async fn record_auth(user_id: i32) -> anyhow::Result<()> {
    let now = Utc::now().naive_utc();

    // sqlx::query(
    //     r#"
    //     INSERT INTO auth_logs (id, last_login)
    //     VALUES ($1, $2)
    //     "#,
    // )
    // .bind(user_id)
    // .bind(now)
    // .execute(POSTGRES.deref())
    // .await?;

    sqlx::query(
        r#"INSERT INTO last_login (id, login_time)
VALUES ($1, $2)
ON CONFLICT (id) 
DO UPDATE SET login_time = EXCLUDED.login_time;
"#,
    )
    .bind(user_id)
    .bind(now)
    .execute(POSTGRES.deref())
    .await?;

    Ok(())
}
