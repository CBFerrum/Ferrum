//! Workspace sign-up (FERRUM-CLOUD-002): someone makes a workspace of their own, when the operator
//! opens sign-up. A code mailed to the address proves it; the workspace starts with the packages
//! and on the plan the operator chose for sign-ups, its maker its administrator. An address makes
//! a few workspaces at most, and the operator may take sign-ups from some domains only. Codes come
//! from the accounts directory, so a cluster takes sign-ups only with a base domain.

use crate::error::{Error, Result};
use crate::identity::normalized_email;
use crate::tenant::NewTenant;
use crate::Kernel;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

/// The operator's sign-up policy.
#[derive(Debug, Clone)]
pub struct SignupPolicy {
    /// Whether anyone may make a workspace.
    pub open: bool,
    /// The address domains sign-ups come from; none for any.
    pub domains: Vec<String>,
    /// What a new workspace installs besides the core.
    pub packages: Vec<String>,
    /// The plan a new workspace is on ([`crate::plans`]).
    pub plan: Option<String>,
    /// How many workspaces one address may make.
    pub per_address: i64,
}

impl Default for SignupPolicy {
    fn default() -> Self {
        SignupPolicy { open: false, domains: Vec::new(), packages: Vec::new(), plan: None, per_address: 3 }
    }
}

/// What the sign-up form sends.
#[derive(Debug, Deserialize)]
pub struct SignUp {
    pub email: String,
    pub code: String,
    /// The workspace's address name (`{slug}.{base}`).
    pub slug: String,
    pub name: String,
    pub password: String,
    #[serde(default)]
    pub full_name: Option<String>,
}

impl Kernel {
    /// The address, when this server takes a sign-up from it.
    fn signup_from(&self, email: &str) -> Result<String> {
        let policy = &self.settings.signup;
        if !policy.open {
            return Err(Error::Forbidden("this server does not take sign-ups: ask its operator for a workspace".into()));
        }
        let email = normalized_email(email)?;
        let domain = email.rsplit('@').next().unwrap_or_default();
        if !policy.domains.is_empty() && !policy.domains.iter().any(|d| d.eq_ignore_ascii_case(domain)) {
            return Err(Error::field("email", "is not at a domain this server takes sign-ups from"));
        }
        Ok(email)
    }

    /// Mails a code to an address that may sign up (at most a few an hour).
    pub async fn send_signup_code(&self, email: &str) -> Result<()> {
        let email = self.signup_from(email)?;
        self.mail_code(&email, "make your Ferrum workspace", false).await
    }

    /// Makes the workspace of someone whose code proves their address; they are its administrator.
    pub async fn sign_up(&self, s: SignUp) -> Result<Uuid> {
        let email = self.signup_from(&s.email)?;
        let new = NewTenant {
            slug: s.slug,
            name: s.name,
            admin_email: email.clone(),
            admin_password: s.password,
            admin_name: s.full_name.filter(|n| !n.trim().is_empty()),
            packages: self.settings.signup.packages.clone(),
            settings: Some(json!({"signed_up_by": email})),
        };
        new.check()?;
        let mut tx = self.db.system_tx().await?;
        let (made, taken): (i64, bool) = sqlx::query_as(
            "select (select count(*) from tenant where settings->>'signed_up_by' = $1), exists(select 1 from tenant where slug = $2)",
        )
        .bind(&email)
        .bind(&new.slug)
        .fetch_one(&mut *tx)
        .await?;
        if made >= self.settings.signup.per_address {
            tx.commit().await?;
            return Err(Error::Forbidden(format!("this address has made {made} workspaces, as many as this server allows")));
        }
        if taken {
            tx.commit().await?;
            return Err(Error::field("slug", "is already taken"));
        }
        // A wrong code counts even when it fails, so the transaction commits either way.
        let proof = self.take_email_code(&mut tx, &email, &s.code).await;
        tx.commit().await?;
        proof?;
        let tenant = self.create_tenant(new).await?;
        if let Some(plan) = &self.settings.signup.plan {
            self.set_plan(tenant, plan).await?;
        }
        tracing::info!(%tenant, "workspace signed up");
        Ok(tenant)
    }
}
