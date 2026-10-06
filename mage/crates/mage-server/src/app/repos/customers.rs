//! Customers and their environments (intake.md customer resolution; storage.md `OrgStore`).

use std::collections::BTreeSet;

use mage_core::domain::{Customer, CustomerId, OrgId};
use mage_core::wire::api::{CustomerInput, CustomerList};

use super::{ReposService, collect_pages};
use crate::error::AppError;

const MAX_NAME_BYTES: usize = 200;
const MAX_ENVIRONMENT_BYTES: usize = 255;
const MAX_REFS: usize = 100;

impl ReposService {
    pub async fn list_customers(&self, org: OrgId) -> Result<CustomerList, AppError> {
        let customers = collect_pages(|page| self.orgs.list_customers(org, page)).await?;
        Ok(CustomerList { customers })
    }

    pub async fn create_customer(&self, org: OrgId, input: CustomerInput) -> Result<Customer, AppError> {
        let customer = customer(self.ids.new_id::<CustomerId>(), org, input)?;
        self.orgs.put_customer(&customer).await?;
        Ok(customer)
    }

    /// Replace name, refs and environment of an existing customer (`NotFound` otherwise).
    pub async fn update_customer(&self, org: OrgId, id: CustomerId, input: CustomerInput) -> Result<Customer, AppError> {
        self.orgs.get_customer(org, id).await?;
        let customer = customer(id, org, input)?;
        self.orgs.put_customer(&customer).await?;
        Ok(customer)
    }
}

/// Validated customer; duplicate refs collapse (a ref names at most one customer, the store
/// rejects a ref held by another customer with `Conflict`).
fn customer(id: CustomerId, org_id: OrgId, input: CustomerInput) -> Result<Customer, AppError> {
    let name = input.name.trim();
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(AppError::Invalid(format!("customer name must be 1..={MAX_NAME_BYTES} bytes")));
    }
    if input.external_refs.len() > MAX_REFS {
        return Err(AppError::Invalid(format!("at most {MAX_REFS} external refs per customer")));
    }
    let environment = match input.environment.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(env) if env.len() > MAX_ENVIRONMENT_BYTES => {
            return Err(AppError::Invalid(format!("environment must be at most {MAX_ENVIRONMENT_BYTES} bytes")));
        }
        Some(env) => Some(env.to_owned()),
    };
    let mut seen = BTreeSet::new();
    let external_refs = input.external_refs.into_iter().filter(|r| seen.insert(r.clone())).collect();
    Ok(Customer { id, org_id, name: name.to_owned(), external_refs, environment })
}
