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


/// Store Azure DevOps PATs separately from Jira credentials, scoped per organization.
/// The PAT is never serialized into the app preferences file.
fn azure_pat_attributes(organization: &str) -> [(&str, &str); 3] {
    [
        ("application", "com.github.ziacik.DeskForJira"),
        ("type", "azure-devops-pat"),
        ("organization", organization),
    ]
}

pub fn load_azure_pat(organization: &str) -> Result<Option<String>> {
    future::block_on(async {
        let keyring = open_keyring().await?;
        let attributes = azure_pat_attributes(organization);
        let items = keyring.search_items(&attributes).await
            .context("Could not search for saved Azure DevOps PAT")?;
        let Some(item) = items.first() else { return Ok(None); };
        let secret = item.secret().await
            .context("Could not read saved Azure DevOps PAT")?;
        let pat = String::from_utf8(secret.as_bytes().to_vec())
            .context("Saved Azure DevOps PAT is not valid UTF-8")?;
        Ok(Some(pat))
    })
}

pub fn save_azure_pat(organization: &str, pat: &str) -> Result<()> {
    future::block_on(async {
        let keyring = open_keyring().await?;
        let attributes = azure_pat_attributes(organization);
        keyring.create_item(
            "Desk for Jira — Azure DevOps PAT",
            &attributes,
            Secret::text(pat),
            true,
        ).await.context("Could not save Azure DevOps PAT to the system keyring")?;
        Ok(())
    })
}

pub fn delete_azure_pat(organization: &str) -> Result<()> {
    future::block_on(async {
        let keyring = open_keyring().await?;
        let attributes = azure_pat_attributes(organization);
        keyring.delete(&attributes).await
            .context("Could not forget saved Azure DevOps PAT")?;
        Ok(())
    })
}

async fn open_keyring() -> Result<Keyring> {
    let keyring = Keyring::new().await.context("Could not open the system keyring")?;
    keyring.unlock().await.context("Could not unlock the system keyring")?;
    Ok(keyring)
}
