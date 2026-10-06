//! Data-dependent membership rules (server.md "RBAC"): pure functions the org use cases call
//! after the route's minimum role has been checked.

use mage_core::domain::MemberRole;

/// Why a membership change is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemberRuleError {
    #[error("cannot invite with a role above your own")]
    InviteAboveCaller,
    #[error("only an Owner can remove an Admin or Owner")]
    RemoveRequiresOwner,
    #[error("only an Owner can change roles")]
    RoleChangeRequiresOwner,
    #[error("the last Owner cannot be removed or demoted")]
    LastOwner,
}

/// UC4 step 1: the invited role is at most the caller's role.
pub fn check_invite(caller: MemberRole, invited: MemberRole) -> Result<(), MemberRuleError> {
    if caller.at_least(invited) { Ok(()) } else { Err(MemberRuleError::InviteAboveCaller) }
}

/// Admins remove Members; Owners remove anyone. `owners` = Owners currently in the org.
pub fn check_remove(caller: MemberRole, target: MemberRole, owners: usize) -> Result<(), MemberRuleError> {
    if target != MemberRole::Member && caller != MemberRole::Owner {
        return Err(MemberRuleError::RemoveRequiresOwner);
    }
    if target == MemberRole::Owner && owners <= 1 {
        return Err(MemberRuleError::LastOwner);
    }
    Ok(())
}

/// Owners change roles; demoting the only Owner is refused.
pub fn check_role_change(caller: MemberRole, current: MemberRole, next: MemberRole, owners: usize) -> Result<(), MemberRuleError> {
    if caller != MemberRole::Owner {
        return Err(MemberRuleError::RoleChangeRequiresOwner);
    }
    if current == MemberRole::Owner && next != MemberRole::Owner && owners <= 1 {
        return Err(MemberRuleError::LastOwner);
    }
    Ok(())
}

/// Reserved org slugs: they collide with top-level UI and API path segments.
const RESERVED_SLUGS: [&str; 12] = ["api", "auth", "assets", "events", "hooks", "runner", "mcp", "llm", "share", "blob", "login", "admin"];

/// Org slugs are immutable URL segments: `[a-z0-9]` and inner `-`, 2..=40 characters.
pub fn valid_org_slug(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    (2..=40).contains(&bytes.len())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes.first().is_some_and(|b| *b != b'-')
        && bytes.last().is_some_and(|b| *b != b'-')
        && !slug.contains("--")
        && !RESERVED_SLUGS.contains(&slug)
}

#[cfg(test)]
mod tests {
    use MemberRole::*;

    use super::*;

    #[test]
    fn invites_never_exceed_the_callers_role() {
        assert_eq!(check_invite(Admin, Admin), Ok(()));
        assert_eq!(check_invite(Admin, Member), Ok(()));
        assert_eq!(check_invite(Admin, Owner), Err(MemberRuleError::InviteAboveCaller));
        assert_eq!(check_invite(Owner, Owner), Ok(()));
        assert_eq!(check_invite(Member, Admin), Err(MemberRuleError::InviteAboveCaller));
    }

    #[test]
    fn removal_needs_an_owner_for_privileged_targets_and_keeps_one_owner() {
        assert_eq!(check_remove(Admin, Member, 1), Ok(()));
        assert_eq!(check_remove(Admin, Admin, 1), Err(MemberRuleError::RemoveRequiresOwner));
        assert_eq!(check_remove(Admin, Owner, 3), Err(MemberRuleError::RemoveRequiresOwner));
        assert_eq!(check_remove(Owner, Admin, 1), Ok(()));
        assert_eq!(check_remove(Owner, Owner, 2), Ok(()));
        assert_eq!(check_remove(Owner, Owner, 1), Err(MemberRuleError::LastOwner));
    }

    #[test]
    fn role_changes_are_owner_only_and_keep_one_owner() {
        assert_eq!(check_role_change(Admin, Member, Admin, 1), Err(MemberRuleError::RoleChangeRequiresOwner));
        assert_eq!(check_role_change(Owner, Member, Owner, 1), Ok(()));
        assert_eq!(check_role_change(Owner, Owner, Admin, 1), Err(MemberRuleError::LastOwner));
        assert_eq!(check_role_change(Owner, Owner, Admin, 2), Ok(()));
        assert_eq!(check_role_change(Owner, Owner, Owner, 1), Ok(()));
    }

    #[test]
    fn org_slugs() {
        for ok in ["hoop", "acme-corp", "a1", "x".repeat(40).as_str()] {
            assert!(valid_org_slug(ok), "{ok}");
        }
        for bad in ["", "a", "Hoop", "-x", "x-", "a--b", "a_b", "api", "assets", "x".repeat(41).as_str(), "a/b"] {
            assert!(!valid_org_slug(bad), "{bad}");
        }
    }
}
