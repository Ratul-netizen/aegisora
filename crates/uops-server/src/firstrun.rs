//! The first boot.
//!
//! [`PgStore::bootstrap_first_run`] decides whether this installation needs an
//! administrator and creates one atomically if it does. This module does the two things
//! that need a process rather than a database: generate the password, and print it.
//!
//! # Printing a credential on purpose
//!
//! Everywhere else in this codebase, a secret reaching a formatting macro is a bug, and
//! CI greps for it. Here it is the entire point — so the one line that does it carries a
//! `credential-print:` marker, the guard skips exactly the lines that carry one, and CI
//! asserts there is still only one in the whole tree. An exemption that has to be
//! written down, and counted, is a different thing from an exemption that can be reached
//! by accident.
//!
//! # Printed to stdout, once
//!
//! Not to a log framework, which would route it to a file, a collector, and eventually
//! this product. Not on every boot — [`PgStore::bootstrap_first_run`] returns `None` the
//! second time, and nothing here runs. If the operator misses it, the recovery is to
//! empty `app_user` and boot again; that is a real cost and it buys an installation with
//! no default credential in it.

use uops_core::Secret;
use uops_secrets::{generate, password};
use uops_store_pg::{FirstRunRequest, PgStore};

use crate::config::FirstRunNames;

/// Bootstrap if the installation is empty, returning the credential to announce.
///
/// Split from [`announce`] so that a test can assert the generated password actually
/// logs in. That assertion is the only one that matters here — a first run that creates
/// an account nobody can authenticate as satisfies every other check and leaves an
/// installation nobody can enter — and it cannot be made against a function whose only
/// output is on a terminal.
///
/// # Errors
///
/// Storage failures, and a hashing failure — both of which must stop the boot. A server
/// that starts after failing to create the only account that can administer it is a
/// server nobody can get into and nothing will tell them why.
pub async fn bootstrap(
    store: &PgStore,
    names: &FirstRunNames,
) -> Result<Option<Secret<String>>, Box<dyn std::error::Error>> {
    // Cheap, racy, and only an optimisation: it saves hashing a password on every boot
    // of a server that has been running for a year. bootstrap_first_run asks again
    // under a lock, and that answer is the one that counts.
    if store.any_user_exists().await? {
        return Ok(None);
    }

    let plaintext = generate::password()?;
    let hash = password::hash(&plaintext)?;

    let created = store
        .bootstrap_first_run(&FirstRunRequest {
            org_name: &names.org,
            tenant_name: &names.tenant,
            tenant_slug: &names.tenant_slug,
            email: &names.admin_email,
            display_name: &names.admin_name,
            password_hash: &hash,
        })
        .await?;

    // None means another replica won the race in the moment between the two checks. It
    // bootstrapped, printed its own password, and this process has nothing to say — in
    // particular it must not print the password it generated, which belongs to no
    // account and would be typed hopefully into a login form until someone gave up.
    Ok(created.map(|_| plaintext))
}

/// Print the credential, once.
///
/// To stdout, not to a log framework, which would route it to a file, a collector and
/// eventually this product.
pub fn announce(names: &FirstRunNames, plaintext: &Secret<String>) {
    println!();
    println!("  ────────────────────────────────────────────────────────────");
    println!("   First run. This is the only time this password is shown.");
    println!("  ────────────────────────────────────────────────────────────");
    println!();
    println!("   organization  {}", names.org);
    println!("   tenant        {} ({})", names.tenant, names.tenant_slug);
    println!("   email         {}", names.admin_email);
    // The one place in this codebase where a credential is written to a terminal on
    // purpose. The marker is on the line itself because that is the line CI greps; a
    // marker in the comment above would let the next one be smuggled in under a
    // borrowed explanation.
    println!("   password      {}", plaintext.expose()); // credential-print: first run
    println!();
    println!("   Sign in and change it. Recovering from a lost first-run password");
    println!("   means emptying app_user and starting this server again.");
    println!();
    println!("  ────────────────────────────────────────────────────────────");
    println!();
}

/// Bootstrap and announce. What the server calls.
///
/// # Errors
///
/// See [`bootstrap`].
pub async fn run(store: &PgStore, names: &FirstRunNames) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(plaintext) = bootstrap(store, names).await? {
        announce(names, &plaintext);
    }
    Ok(())
}
