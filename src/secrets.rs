use anyhow::{Context, Result};
use futures_lite::future;
use oo7::{Keyring, Secret};

use crate::jira::Credentials;

const ATTRIBUTES: [(&str, &str); 2] = [
    ("application", "com.github.ziacik.DeskForJira"),
    ("type", "jira-credentials"),
];

pub fn load() -> Result<Option<Credentials>> {
    future::block_on(async {
        let keyring = open_keyring().await?;
        let items = keyring
            .search_items(&ATTRIBUTES)
            .await
            .context("Could not search the system keyring")?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        let secret = item.secret().await.context("Could not read saved account")?;
        let account = serde_json::from_slice(secret.as_bytes())
            .context("Saved Jira account has an invalid format")?;
        Ok(Some(account))
    })
}

pub fn save(account: &Credentials) -> Result<()> {
    let serialized = serde_json::to_string(account)?;
    future::block_on(async {
        open_keyring()
            .await?
            .create_item("Desk for Jira account", &ATTRIBUTES, Secret::text(&serialized), true)
            .await
            .context("Could not store the account in the system keyring")?;
        Ok(())
    })
}

pub fn delete() -> Result<()> {
    future::block_on(async {
        open_keyring()
            .await?
            .delete(&ATTRIBUTES)
            .await
            .context("Could not remove the saved Jira account")?;
        Ok(())
    })
}

async fn open_keyring() -> Result<Keyring> {
    let keyring = Keyring::new().await.context("Could not open the system keyring")?;
    keyring.unlock().await.context("Could not unlock the system keyring")?;
    Ok(keyring)
}
