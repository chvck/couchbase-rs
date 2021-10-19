use std::time::Duration;

#[derive(Debug, Default)]
pub struct MutateInOptions {
    pub(crate) timeout: Option<Duration>,
    pub(crate) cas: Option<u64>,
    pub(crate) store_semantics: Option<StoreSemantics>,
    pub(crate) expiry: Option<Duration>,
    pub(crate) access_deleted: Option<bool>,
    pub(crate) preserve_expiry: bool,
}

impl MutateInOptions {
    timeout!();
    expiry!();
    preserve_expiry!();

    pub fn cas(mut self, cas: u64) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn store_semantics(mut self, store_semantics: StoreSemantics) -> Self {
        self.store_semantics = Some(store_semantics);
        self
    }

    pub fn access_deleted(mut self, access_deleted: bool) -> Self {
        self.access_deleted = Some(access_deleted);
        self
    }
}

/// Describes how the outer document store semantics on subdoc should act.
#[derive(Debug)]
pub enum StoreSemantics {
    /// Create the document, fail if it exists.
    Insert,
    /// Replace the document or create it if it does not exist.
    Upsert,
    /// Replace the document, fail if it does not exist. This is the default.
    Replace,
}

#[derive(Debug, Default)]
pub struct LookupInOptions {
    pub(crate) timeout: Option<Duration>,
    pub(crate) access_deleted: Option<bool>,
}

impl LookupInOptions {
    timeout!();

    pub fn access_deleted(mut self, access_deleted: bool) -> Self {
        self.access_deleted = Some(access_deleted);
        self
    }
}

#[derive(Debug, Default)]
pub struct InsertSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl InsertSpecOptions {
    xattr!();
    pub fn new() -> Self {
        InsertSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct UpsertSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl UpsertSpecOptions {
    xattr!();
    pub fn new() -> Self {
        UpsertSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct ReplaceSpecOptions {
    pub(crate) xattr: bool,
}

impl ReplaceSpecOptions {
    xattr!();
    pub fn new() -> Self {
        ReplaceSpecOptions::default()
    }
}

#[derive(Debug, Default)]
pub struct RemoveSpecOptions {
    pub(crate) xattr: bool,
}

impl RemoveSpecOptions {
    xattr!();
    pub fn new() -> Self {
        RemoveSpecOptions::default()
    }
}

#[derive(Debug, Default)]
pub struct ArrayAppendSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl ArrayAppendSpecOptions {
    xattr!();
    pub fn new() -> Self {
        ArrayAppendSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct ArrayPrependSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl ArrayPrependSpecOptions {
    xattr!();
    pub fn new() -> Self {
        ArrayPrependSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct ArrayInsertSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl ArrayInsertSpecOptions {
    xattr!();
    pub fn new() -> Self {
        ArrayInsertSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct ArrayAddUniqueSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl ArrayAddUniqueSpecOptions {
    xattr!();
    pub fn new() -> Self {
        ArrayAddUniqueSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct IncrementSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl IncrementSpecOptions {
    xattr!();
    pub fn new() -> Self {
        IncrementSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct DecrementSpecOptions {
    pub(crate) create_path: bool,
    pub(crate) xattr: bool,
}

impl DecrementSpecOptions {
    xattr!();
    pub fn new() -> Self {
        DecrementSpecOptions::default()
    }

    pub fn create_path(mut self, create: bool) -> Self {
        self.create_path = create;
        self
    }
}

#[derive(Debug, Default)]
pub struct GetSpecOptions {
    pub(crate) xattr: bool,
}

impl GetSpecOptions {
    xattr!();
    pub fn new() -> Self {
        GetSpecOptions::default()
    }
}

#[derive(Debug, Default)]
pub struct ExistsSpecOptions {
    pub(crate) xattr: bool,
}

impl ExistsSpecOptions {
    xattr!();
    pub fn new() -> Self {
        ExistsSpecOptions::default()
    }
}

#[derive(Debug, Default)]
pub struct CountSpecOptions {
    pub(crate) xattr: bool,
}

impl CountSpecOptions {
    xattr!();
    pub fn new() -> Self {
        CountSpecOptions::default()
    }
}
