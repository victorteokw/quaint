mod conversion;
mod error;

use crate::{
    ast::{Query, Value},
    connector::{metrics, queryable::*, ResultSet, Transaction},
    error::{Error, ErrorKind},
    visitor::{self, Visitor},
};
use async_trait::async_trait;
use futures::{future::FutureExt, lock::Mutex};
use lru_cache::LruCache;
use native_tls::{Certificate, Identity, TlsConnector};
use percent_encoding::percent_decode;
use postgres_native_tls::MakeTlsConnector;
use std::{
    borrow::{Borrow, Cow},
    fmt::{Debug, Display},
    fs,
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio_postgres::{
    config::{ChannelBinding, SslMode},
    Client, Config, Statement,
};
use url::Url;

pub(crate) const DEFAULT_SCHEMA: &str = "public";

/// The underlying postgres driver. Only available with the `expose-drivers`
/// Cargo feature.
#[cfg(feature = "expose-drivers")]
pub use tokio_postgres;

use super::IsolationLevel;

#[derive(Clone)]
struct Hidden<T>(T);

impl<T> Debug for Hidden<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<HIDDEN>")
    }
}

struct PostgresClient(Client);

impl Debug for PostgresClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PostgresClient")
    }
}

/// A connector interface for the PostgreSQL database.
#[derive(Debug)]
pub struct PostgreSql {
    client: PostgresClient,
    pg_bouncer: bool,
    socket_timeout: Option<Duration>,
    statement_cache: Mutex<LruCache<String, Statement>>,
    is_healthy: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslAcceptMode {
    Strict,
    AcceptInvalidCerts,
}

#[derive(Debug, Clone)]
pub struct SslParams {
    certificate_file: Option<String>,
    identity_file: Option<String>,
    identity_password: Hidden<Option<String>>,
    ssl_accept_mode: SslAcceptMode,
}

#[derive(Debug)]
struct SslAuth {
    certificate: Hidden<Option<Certificate>>,
    identity: Hidden<Option<Identity>>,
    ssl_accept_mode: SslAcceptMode,
}

impl Default for SslAuth {
    fn default() -> Self {
        Self {
            certificate: Hidden(None),
            identity: Hidden(None),
            ssl_accept_mode: SslAcceptMode::AcceptInvalidCerts,
        }
    }
}

impl SslAuth {
    fn certificate(&mut self, certificate: Certificate) -> &mut Self {
        self.certificate = Hidden(Some(certificate));
        self
    }

    fn identity(&mut self, identity: Identity) -> &mut Self {
        self.identity = Hidden(Some(identity));
        self
    }

    fn accept_mode(&mut self, mode: SslAcceptMode) -> &mut Self {
        self.ssl_accept_mode = mode;
        self
    }
}

impl SslParams {
    async fn into_auth(self) -> crate::Result<SslAuth> {
        let mut auth = SslAuth::default();
        auth.accept_mode(self.ssl_accept_mode);

        if let Some(ref cert_file) = self.certificate_file {
            let cert = fs::read(cert_file).map_err(|err| {
                Error::builder(ErrorKind::TlsError {
                    message: format!("cert file not found ({err})"),
                })
                .build()
            })?;

            auth.certificate(Certificate::from_pem(&cert)?);
        }

        if let Some(ref identity_file) = self.identity_file {
            let db = fs::read(identity_file).map_err(|err| {
                Error::builder(ErrorKind::TlsError {
                    message: format!("identity file not found ({err})"),
                })
                .build()
            })?;
            let password = self.identity_password.0.as_deref().unwrap_or("");
            let identity = Identity::from_pkcs12(&db, password)?;

            auth.identity(identity);
        }

        Ok(auth)
    }
}

/// Wraps a connection url and exposes the parsing logic used by Quaint,
/// including default values.
#[derive(Debug, Clone)]
pub struct PostgresUrl {
    url: Url,
    query_params: PostgresUrlQueryParams,
}

impl PostgresUrl {
    /// Parse `Url` to `PostgresUrl`. Returns error for mistyped connection
    /// parameters.
    pub fn new(url: Url) -> Result<Self, Error> {
        let query_params = Self::parse_query_params(&url)?;

        Ok(Self { url, query_params })
    }

    /// The bare `Url` to the database.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The percent-decoded database username.
    pub fn username(&self) -> Cow<str> {
        match percent_decode(self.url.username().as_bytes()).decode_utf8() {
            Ok(username) => username,
            Err(_) => {
                tracing::warn!("Couldn't decode username to UTF-8, using the non-decoded version.");

                self.url.username().into()
            }
        }
    }

    /// The database host. Taken first from the `host` query parameter, then
    /// from the `host` part of the URL. For socket connections, the query
    /// parameter must be used.
    ///
    /// If none of them are set, defaults to `localhost`.
    pub fn host(&self) -> &str {
        match (self.query_params.host.as_ref(), self.url.host_str()) {
            (Some(host), _) => host.as_str(),
            (None, Some("")) => "localhost",
            (None, None) => "localhost",
            (None, Some(host)) => host,
        }
    }

    /// Name of the database connected. Defaults to `postgres`.
    pub fn dbname(&self) -> &str {
        match self.url.path_segments() {
            Some(mut segments) => segments.next().unwrap_or("postgres"),
            None => "postgres",
        }
    }

    /// The percent-decoded database password.
    pub fn password(&self) -> Cow<str> {
        match self
            .url
            .password()
            .and_then(|pw| percent_decode(pw.as_bytes()).decode_utf8().ok())
        {
            Some(password) => password,
            None => self.url.password().unwrap_or("").into(),
        }
    }

    /// The database port, defaults to `5432`.
    pub fn port(&self) -> u16 {
        self.url.port().unwrap_or(5432)
    }

    /// The database schema, defaults to `public`.
    pub fn schema(&self) -> &str {
        self.query_params.schema.as_deref().unwrap_or(DEFAULT_SCHEMA)
    }

    /// Whether the pgbouncer mode is enabled.
    pub fn pg_bouncer(&self) -> bool {
        self.query_params.pg_bouncer
    }

    /// The connection timeout.
    pub fn connect_timeout(&self) -> Option<Duration> {
        self.query_params.connect_timeout
    }

    /// Pool check_out timeout
    pub fn pool_timeout(&self) -> Option<Duration> {
        self.query_params.pool_timeout
    }

    /// The socket timeout
    pub fn socket_timeout(&self) -> Option<Duration> {
        self.query_params.socket_timeout
    }

    /// The maximum connection lifetime
    pub fn max_connection_lifetime(&self) -> Option<Duration> {
        self.query_params.max_connection_lifetime
    }

    /// The maximum idle connection lifetime
    pub fn max_idle_connection_lifetime(&self) -> Option<Duration> {
        self.query_params.max_idle_connection_lifetime
    }

    /// The custom application name
    pub fn application_name(&self) -> Option<&str> {
        self.query_params.application_name.as_deref()
    }

    pub fn channel_binding(&self) -> ChannelBinding {
        self.query_params.channel_binding
    }

    pub(crate) fn cache(&self) -> LruCache<String, Statement> {
        if self.query_params.pg_bouncer {
            LruCache::new(0)
        } else {
            LruCache::new(self.query_params.statement_cache_size)
        }
    }

    pub(crate) fn options(&self) -> Option<&str> {
        self.query_params.options.as_deref()
    }

    fn parse_query_params(url: &Url) -> Result<PostgresUrlQueryParams, Error> {
        let mut connection_limit = None;
        let mut schema = None;
        let mut certificate_file = None;
        let mut identity_file = None;
        let mut identity_password = None;
        let mut ssl_accept_mode = SslAcceptMode::AcceptInvalidCerts;
        let mut ssl_mode = SslMode::Prefer;
        let mut host = None;
        let mut application_name = None;
        let mut channel_binding = ChannelBinding::Prefer;
        let mut socket_timeout = None;
        let mut connect_timeout = Some(Duration::from_secs(5));
        let mut pool_timeout = Some(Duration::from_secs(10));
        let mut pg_bouncer = false;
        let mut statement_cache_size = 100;
        let mut max_connection_lifetime = None;
        let mut max_idle_connection_lifetime = Some(Duration::from_secs(300));
        let mut options = None;

        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "pgbouncer" => {
                    pg_bouncer = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;
                }
                "sslmode" => {
                    match v.as_ref() {
                        "disable" => ssl_mode = SslMode::Disable,
                        "prefer" => ssl_mode = SslMode::Prefer,
                        "require" => ssl_mode = SslMode::Require,
                        _ => {
                            tracing::debug!(message = "Unsupported SSL mode, defaulting to `prefer`", mode = &*v);
                        }
                    };
                }
                "sslcert" => {
                    certificate_file = Some(v.to_string());
                }
                "sslidentity" => {
                    identity_file = Some(v.to_string());
                }
                "sslpassword" => {
                    identity_password = Some(v.to_string());
                }
                "statement_cache_size" => {
                    statement_cache_size = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;
                }
                "sslaccept" => {
                    match v.as_ref() {
                        "strict" => {
                            ssl_accept_mode = SslAcceptMode::Strict;
                        }
                        "accept_invalid_certs" => {
                            ssl_accept_mode = SslAcceptMode::AcceptInvalidCerts;
                        }
                        _ => {
                            tracing::debug!(
                                message = "Unsupported SSL accept mode, defaulting to `strict`",
                                mode = &*v
                            );

                            ssl_accept_mode = SslAcceptMode::Strict;
                        }
                    };
                }
                "schema" => {
                    schema = Some(v.to_string());
                }
                "connection_limit" => {
                    let as_int: usize = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;
                    connection_limit = Some(as_int);
                }
                "host" => {
                    host = Some(v.to_string());
                }
                "socket_timeout" => {
                    let as_int = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;
                    socket_timeout = Some(Duration::from_secs(as_int));
                }
                "connect_timeout" => {
                    let as_int = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;

                    if as_int == 0 {
                        connect_timeout = None;
                    } else {
                        connect_timeout = Some(Duration::from_secs(as_int));
                    }
                }
                "pool_timeout" => {
                    let as_int = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;

                    if as_int == 0 {
                        pool_timeout = None;
                    } else {
                        pool_timeout = Some(Duration::from_secs(as_int));
                    }
                }
                "max_connection_lifetime" => {
                    let as_int = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;

                    if as_int == 0 {
                        max_connection_lifetime = None;
                    } else {
                        max_connection_lifetime = Some(Duration::from_secs(as_int));
                    }
                }
                "max_idle_connection_lifetime" => {
                    let as_int = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;

                    if as_int == 0 {
                        max_idle_connection_lifetime = None;
                    } else {
                        max_idle_connection_lifetime = Some(Duration::from_secs(as_int));
                    }
                }
                "application_name" => {
                    application_name = Some(v.to_string());
                }
                "channel_binding" => {
                    match v.as_ref() {
                        "disable" => channel_binding = ChannelBinding::Disable,
                        "prefer" => channel_binding = ChannelBinding::Prefer,
                        "require" => channel_binding = ChannelBinding::Require,
                        _ => {
                            tracing::debug!(
                                message = "Unsupported Channel Binding {channel_binding}, defaulting to `prefer`",
                                channel_binding = &*v
                            );
                        }
                    };
                }
                "options" => {
                    options = Some(v.to_string());
                }
                _ => {
                    tracing::trace!(message = "Discarding connection string param", param = &*k);
                }
            };
        }

        Ok(PostgresUrlQueryParams {
            ssl_params: SslParams {
                certificate_file,
                identity_file,
                ssl_accept_mode,
                identity_password: Hidden(identity_password),
            },
            connection_limit,
            schema,
            ssl_mode,
            host,
            connect_timeout,
            pool_timeout,
            socket_timeout,
            pg_bouncer,
            statement_cache_size,
            max_connection_lifetime,
            max_idle_connection_lifetime,
            application_name,
            channel_binding,
            options,
        })
    }

    pub(crate) fn ssl_params(&self) -> &SslParams {
        &self.query_params.ssl_params
    }

    #[cfg(feature = "pooled")]
    pub(crate) fn connection_limit(&self) -> Option<usize> {
        self.query_params.connection_limit
    }

    pub(crate) fn to_config(&self) -> Config {
        let mut config = Config::new();

        config.user(self.username().borrow() as &str);
        config.password(self.password().borrow() as &str);
        config.host(self.host());
        config.port(self.port());
        config.dbname(self.dbname());
        // config.pgbouncer_mode(self.query_params.pg_bouncer);

        if let Some(options) = self.options() {
            config.options(options);
        }

        if let Some(application_name) = self.application_name() {
            config.application_name(application_name);
        }

        if let Some(connect_timeout) = self.query_params.connect_timeout {
            config.connect_timeout(connect_timeout);
        };

        config.ssl_mode(self.query_params.ssl_mode);

        config.channel_binding(self.query_params.channel_binding);

        config
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresUrlQueryParams {
    ssl_params: SslParams,
    connection_limit: Option<usize>,
    schema: Option<String>,
    ssl_mode: SslMode,
    pg_bouncer: bool,
    host: Option<String>,
    socket_timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    pool_timeout: Option<Duration>,
    statement_cache_size: usize,
    max_connection_lifetime: Option<Duration>,
    max_idle_connection_lifetime: Option<Duration>,
    application_name: Option<String>,
    channel_binding: ChannelBinding,
    options: Option<String>,
}

impl PostgreSql {
    /// Create a new connection to the database.
    pub async fn new(url: PostgresUrl) -> crate::Result<Self> {
        let config = url.to_config();

        let mut tls_builder = TlsConnector::builder();

        {
            let ssl_params = url.ssl_params();
            let auth = ssl_params.to_owned().into_auth().await?;

            if let Some(certificate) = auth.certificate.0 {
                tls_builder.add_root_certificate(certificate);
            }

            tls_builder.danger_accept_invalid_certs(auth.ssl_accept_mode == SslAcceptMode::AcceptInvalidCerts);

            if let Some(identity) = auth.identity.0 {
                tls_builder.identity(identity);
            }
        }

        let tls = MakeTlsConnector::new(tls_builder.build()?);
        let (client, conn) = super::timeout::connect(url.connect_timeout(), config.connect(tls)).await?;

        tokio::spawn(conn.map(|r| match r {
            Ok(_) => (),
            Err(e) => {
                tracing::error!("Error in PostgreSQL connection: {:?}", e);
            }
        }));

        // SET NAMES sets the client text encoding. It needs to be explicitly set for automatic
        // conversion to and from UTF-8 to happen server-side.
        //
        // Relevant docs: https://www.postgresql.org/docs/current/multibyte.html
        let session_variables = format!(
            r##"
            {set_search_path}
            SET NAMES 'UTF8';
            "##,
            set_search_path = SetSearchPath(url.query_params.schema.as_deref())
        );

        client.simple_query(session_variables.as_str()).await?;

        Ok(Self {
            client: PostgresClient(client),
            socket_timeout: url.query_params.socket_timeout,
            pg_bouncer: url.query_params.pg_bouncer,
            statement_cache: Mutex::new(url.cache()),
            is_healthy: AtomicBool::new(true),
        })
    }

    /// The underlying tokio_postgres::Client. Only available with the
    /// `expose-drivers` Cargo feature. This is a lower level API when you need
    /// to get into database specific features.
    #[cfg(feature = "expose-drivers")]
    pub fn client(&self) -> &tokio_postgres::Client {
        &self.client.0
    }

    async fn fetch_cached(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<Statement> {
        let mut cache = self.statement_cache.lock().await;
        let capacity = cache.capacity();
        let stored = cache.len();

        match cache.get_mut(sql) {
            Some(stmt) => {
                tracing::trace!(
                    message = "CACHE HIT!",
                    query = sql,
                    capacity = capacity,
                    stored = stored,
                );

                Ok(stmt.clone()) // arc'd
            }
            None => {
                tracing::trace!(
                    message = "CACHE MISS!",
                    query = sql,
                    capacity = capacity,
                    stored = stored,
                );

                let param_types = conversion::params_to_types(params);
                let stmt = self.perform_io(self.client.0.prepare_typed(sql, &param_types)).await?;

                cache.insert(sql.to_string(), stmt.clone());

                Ok(stmt)
            }
        }
    }

    async fn perform_io<F, T>(&self, fut: F) -> crate::Result<T>
    where
        F: Future<Output = Result<T, tokio_postgres::Error>>,
    {
        match super::timeout::socket(self.socket_timeout, fut).await {
            Err(e) if e.is_closed() => {
                self.is_healthy.store(false, Ordering::SeqCst);
                Err(e)
            }
            res => res,
        }
    }

    fn check_bind_variables_len(&self, params: &[Value<'_>]) -> crate::Result<()> {
        if params.len() > i16::MAX as usize {
            // tokio_postgres would return an error here. Let's avoid calling the driver
            // and return an error early.
            let kind = ErrorKind::QueryInvalidInput(format!(
                "too many bind variables in prepared statement, expected maximum of {}, received {}",
                i16::MAX,
                params.len()
            ));
            Err(Error::builder(kind).build())
        } else {
            Ok(())
        }
    }
}

// A SetSearchPath statement (Display-impl) for connection initialization.
struct SetSearchPath<'a>(Option<&'a str>);

impl Display for SetSearchPath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(schema) = self.0 {
            f.write_str("SET search_path = \"")?;
            f.write_str(schema)?;
            f.write_str("\";\n")?;
        }

        Ok(())
    }
}

impl TransactionCapable for PostgreSql {}

#[async_trait]
impl Queryable for PostgreSql {
    async fn query(&self, q: Query<'_>) -> crate::Result<ResultSet> {
        let (sql, params) = visitor::Postgres::build(q)?;

        self.query_raw(sql.as_str(), &params[..]).await
    }

    async fn query_raw(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<ResultSet> {
        self.check_bind_variables_len(params)?;

        metrics::query("postgres.query_raw", sql, params, move || async move {
            let stmt = self.fetch_cached(sql, &[]).await?;

            if stmt.params().len() != params.len() {
                let kind = ErrorKind::IncorrectNumberOfParameters {
                    expected: stmt.params().len(),
                    actual: params.len(),
                };

                return Err(Error::builder(kind).build());
            }

            let rows = self
                .perform_io(self.client.0.query(&stmt, conversion::conv_params(params).as_slice()))
                .await?;

            let mut result = ResultSet::new(stmt.to_column_names(), Vec::new());

            for row in rows {
                result.rows.push(row.get_result_row()?);
            }

            Ok(result)
        })
        .await
    }

    async fn query_raw_typed(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<ResultSet> {
        self.check_bind_variables_len(params)?;

        metrics::query("postgres.query_raw", sql, params, move || async move {
            let stmt = self.fetch_cached(sql, params).await?;

            if stmt.params().len() != params.len() {
                let kind = ErrorKind::IncorrectNumberOfParameters {
                    expected: stmt.params().len(),
                    actual: params.len(),
                };

                return Err(Error::builder(kind).build());
            }

            let rows = self
                .perform_io(self.client.0.query(&stmt, conversion::conv_params(params).as_slice()))
                .await?;

            let mut result = ResultSet::new(stmt.to_column_names(), Vec::new());

            for row in rows {
                result.rows.push(row.get_result_row()?);
            }

            Ok(result)
        })
        .await
    }

    async fn execute(&self, q: Query<'_>) -> crate::Result<u64> {
        let (sql, params) = visitor::Postgres::build(q)?;

        self.execute_raw(sql.as_str(), &params[..]).await
    }

    async fn execute_raw(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<u64> {
        self.check_bind_variables_len(params)?;

        metrics::query("postgres.execute_raw", sql, params, move || async move {
            let stmt = self.fetch_cached(sql, &[]).await?;

            if stmt.params().len() != params.len() {
                let kind = ErrorKind::IncorrectNumberOfParameters {
                    expected: stmt.params().len(),
                    actual: params.len(),
                };

                return Err(Error::builder(kind).build());
            }

            let changes = self
                .perform_io(self.client.0.execute(&stmt, conversion::conv_params(params).as_slice()))
                .await?;

            Ok(changes)
        })
        .await
    }

    async fn execute_raw_typed(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<u64> {
        self.check_bind_variables_len(params)?;

        metrics::query("postgres.execute_raw", sql, params, move || async move {
            let stmt = self.fetch_cached(sql, params).await?;

            if stmt.params().len() != params.len() {
                let kind = ErrorKind::IncorrectNumberOfParameters {
                    expected: stmt.params().len(),
                    actual: params.len(),
                };

                return Err(Error::builder(kind).build());
            }

            let changes = self
                .perform_io(self.client.0.execute(&stmt, conversion::conv_params(params).as_slice()))
                .await?;

            Ok(changes)
        })
        .await
    }

    async fn raw_cmd(&self, cmd: &str) -> crate::Result<()> {
        metrics::query("postgres.raw_cmd", cmd, &[], move || async move {
            self.perform_io(self.client.0.simple_query(cmd)).await?;
            Ok(())
        })
        .await
    }

    async fn version(&self) -> crate::Result<Option<String>> {
        let query = r#"SELECT version()"#;
        let rows = self.query_raw(query, &[]).await?;

        let version_string = rows
            .get(0)
            .and_then(|row| row.get("version").and_then(|version| version.to_string()));

        Ok(version_string)
    }

    fn is_healthy(&self) -> bool {
        self.is_healthy.load(Ordering::SeqCst)
    }

    async fn server_reset_query(&self, tx: &Transaction<'_>) -> crate::Result<()> {
        if self.pg_bouncer {
            tx.raw_cmd("DEALLOCATE ALL").await
        } else {
            Ok(())
        }
    }

    async fn set_tx_isolation_level(&self, isolation_level: IsolationLevel) -> crate::Result<()> {
        if matches!(isolation_level, IsolationLevel::Snapshot) {
            return Err(Error::builder(ErrorKind::invalid_isolation_level(&isolation_level)).build());
        }

        self.raw_cmd(&format!("SET TRANSACTION ISOLATION LEVEL {isolation_level}"))
            .await?;

        Ok(())
    }

    fn requires_isolation_first(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_api::postgres::CONN_STR;
    use crate::{connector::Queryable, error::*, single::Quaint};
    use url::Url;

    #[test]
    fn should_parse_socket_url() {
        let url = PostgresUrl::new(Url::parse("postgresql:///dbname?host=/var/run/psql.sock").unwrap()).unwrap();
        assert_eq!("dbname", url.dbname());
        assert_eq!("/var/run/psql.sock", url.host());
    }

    #[test]
    fn should_parse_escaped_url() {
        let url = PostgresUrl::new(Url::parse("postgresql:///dbname?host=%2Fvar%2Frun%2Fpostgresql").unwrap()).unwrap();
        assert_eq!("dbname", url.dbname());
        assert_eq!("/var/run/postgresql", url.host());
    }

    #[test]
    fn should_allow_changing_of_cache_size() {
        let url =
            PostgresUrl::new(Url::parse("postgresql:///localhost:5432/foo?statement_cache_size=420").unwrap()).unwrap();
        assert_eq!(420, url.cache().capacity());
    }

    #[test]
    fn should_have_default_cache_size() {
        let url = PostgresUrl::new(Url::parse("postgresql:///localhost:5432/foo").unwrap()).unwrap();
        assert_eq!(100, url.cache().capacity());
    }

    #[test]
    fn should_have_application_name() {
        let url =
            PostgresUrl::new(Url::parse("postgresql:///localhost:5432/foo?application_name=test").unwrap()).unwrap();
        assert_eq!(Some("test"), url.application_name());
    }

    #[test]
    fn should_have_channel_binding() {
        let url =
            PostgresUrl::new(Url::parse("postgresql:///localhost:5432/foo?channel_binding=require").unwrap()).unwrap();
        assert_eq!(ChannelBinding::Require, url.channel_binding());
    }

    #[test]
    fn should_have_default_channel_binding() {
        let url =
            PostgresUrl::new(Url::parse("postgresql:///localhost:5432/foo?channel_binding=invalid").unwrap()).unwrap();
        assert_eq!(ChannelBinding::Prefer, url.channel_binding());

        let url = PostgresUrl::new(Url::parse("postgresql:///localhost:5432/foo").unwrap()).unwrap();
        assert_eq!(ChannelBinding::Prefer, url.channel_binding());
    }

    #[test]
    fn should_not_enable_caching_with_pgbouncer() {
        let url = PostgresUrl::new(Url::parse("postgresql:///localhost:5432/foo?pgbouncer=true").unwrap()).unwrap();
        assert_eq!(0, url.cache().capacity());
    }

    #[test]
    fn should_parse_default_host() {
        let url = PostgresUrl::new(Url::parse("postgresql:///dbname").unwrap()).unwrap();
        assert_eq!("dbname", url.dbname());
        assert_eq!("localhost", url.host());
    }

    #[test]
    fn should_handle_options_field() {
        let url = PostgresUrl::new(Url::parse("postgresql:///localhost:5432?options=--cluster%3Dmy_cluster").unwrap())
            .unwrap();

        assert_eq!("--cluster=my_cluster", url.options().unwrap());
    }

    #[tokio::test]
    async fn test_custom_search_path() {
        let mut url = Url::parse(&CONN_STR).unwrap();
        url.query_pairs_mut().append_pair("schema", "musti-test");

        let client = Quaint::new(url.as_str()).await.unwrap();

        let result_set = client.query_raw("SHOW search_path", &[]).await.unwrap();
        let row = result_set.first().unwrap();

        assert_eq!(Some("\"musti-test\""), row[0].as_str());
    }

    #[tokio::test]
    async fn should_map_nonexisting_database_error() {
        let mut url = Url::parse(&CONN_STR).unwrap();
        url.set_path("/this_does_not_exist");

        let res = Quaint::new(url.as_str()).await;

        assert!(res.is_err());

        match res {
            Ok(_) => unreachable!(),
            Err(e) => match e.kind() {
                ErrorKind::DatabaseDoesNotExist { db_name } => {
                    assert_eq!(Some("3D000"), e.original_code());
                    assert_eq!(
                        Some("database \"this_does_not_exist\" does not exist"),
                        e.original_message()
                    );
                    assert_eq!(&Name::available("this_does_not_exist"), db_name)
                }
                kind => panic!("Expected `DatabaseDoesNotExist`, got {:?}", kind),
            },
        }
    }

    #[tokio::test]
    async fn should_map_wrong_credentials_error() {
        let mut url = Url::parse(&CONN_STR).unwrap();
        url.set_username("WRONG").unwrap();

        let res = Quaint::new(url.as_str()).await;
        assert!(res.is_err());

        let err = res.unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::AuthenticationFailed { user } if user == &Name::available("WRONG")));
    }

    #[tokio::test]
    async fn should_map_tls_errors() {
        let mut url = Url::parse(&CONN_STR).expect("parsing url");
        url.set_query(Some("sslmode=require&sslaccept=strict"));

        let res = Quaint::new(url.as_str()).await;

        assert!(res.is_err());

        match res {
            Ok(_) => unreachable!(),
            Err(e) => match e.kind() {
                ErrorKind::TlsError { .. } => (),
                other => panic!("{:#?}", other),
            },
        }
    }

    #[tokio::test]
    async fn should_map_incorrect_parameters_error() {
        let url = Url::parse(&CONN_STR).unwrap();
        let conn = Quaint::new(url.as_str()).await.unwrap();

        let res = conn
            .query_raw("SELECT $1", &[Value::integer(1), Value::integer(2)])
            .await;

        assert!(res.is_err());

        match res {
            Ok(_) => unreachable!(),
            Err(e) => match e.kind() {
                ErrorKind::IncorrectNumberOfParameters { expected, actual } => {
                    assert_eq!(1, *expected);
                    assert_eq!(2, *actual);
                }
                other => panic!("{:#?}", other),
            },
        }
    }
}
