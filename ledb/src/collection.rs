use std::{
    cmp::Ordering,
    collections::HashSet,
    marker::PhantomData,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc, RwLock,
    },
};

use lmdb::{
    put::Flags as PutFlags, traits::CreateCursor, Cursor, CursorIter, Database, DatabaseOptions,
    LmdbResultExt, MaybeOwned, ReadTransaction, Unaligned, WriteTransaction,
};
use ron::ser::to_string as to_db_name;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use supercow::{ext::ConstDeref, Supercow};

use super::{
    DatabaseDef, Document, Enumerable, Filter, Index, IndexDef, IndexKind, KeyField, KeyFields,
    KeyType, Modify, Order, OrderKind, Primary, RawDocument, Result, ResultWrap, Serial, Storage,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectionDef(
    /// Unique serial
    pub Serial,
    /// Collection name
    pub String,
);

impl CollectionDef {
    pub fn new<S: AsRef<str>>(name: S) -> Self {
        CollectionDef(0, name.as_ref().into())
    }
}

impl Enumerable for CollectionDef {
    fn enumerate(&mut self, serial: Serial) {
        self.0 = serial;
    }
}

struct CollectionData {
    name: String,
    indexes: RwLock<Vec<Index>>,
    storage: Storage,
    db: Database<'static>,
    // Remove marker
    delete: AtomicBool,
}

/// Collection of documents
#[derive(Clone)]
pub struct Collection(Option<Arc<CollectionData>>);

impl Collection {
    pub(crate) fn new(
        storage: Storage,
        def: CollectionDef,
        index_defs: Vec<IndexDef>,
    ) -> Result<Self> {
        let db_name = to_db_name(&DatabaseDef::Collection(def.clone())).wrap_err()?;

        let CollectionDef(_serial, name) = def;

        let db_opts = DatabaseOptions::create_map::<Unaligned<Primary>>();

        let db = Database::open(storage.clone(), Some(&db_name), &db_opts).wrap_err()?;

        let indexes = RwLock::new(
            index_defs
                .into_iter()
                .map(|def| Index::new(storage.clone(), def))
                .collect::<Result<Vec<_>>>()?,
        );

        Ok(Collection(Some(Arc::new(CollectionData {
            name,
            indexes,
            storage,
            db,
            delete: AtomicBool::new(false),
        }))))
    }

    fn handle(&self) -> &CollectionData {
        if let Some(ref handle) = self.0 {
            handle
        } else {
            unreachable!();
        }
    }

    pub fn name(&self) -> &str {
        &self.handle().name
    }

    /// Insert document into collection
    ///
    /// The primary key/identifier of document will be selected by auto incrementing the id of last inserted document.
    ///
    /// Primary key/identifier of new inserted document will be returned.
    ///
    pub fn insert<T: Serialize + Document>(&self, doc: T) -> Result<Primary> {
        let id = self.new_id()?;

        self.put_raw(RawDocument::from_doc(&doc)?.with_id(id))?;

        Ok(id)
    }

    /// Find documents using optional filter and ordering
    ///
    /// When none filter specified then all documents will be found.
    ///
    /// Iterator across found documents will be returned.
    ///
    /// You can use `DocumentsIterator::len()` for getting the total number of found documents.
    ///
    pub fn find<T: DeserializeOwned + Document>(
        &self,
        filter: Option<Filter>,
        order: Order,
    ) -> Result<DocumentsIterator<T>> {
        let handle = if let Some(handle) = &self.0 {
            handle
        } else {
            unreachable!();
        };

        let txn = Arc::new(ReadTransaction::new(handle.storage.clone())?);

        let ids = match (filter, order) {
            (None, Order::Primary(order)) => {
                PrimaryIterator::new(txn.clone(), self.clone(), order)?
                    .collect::<Result<Vec<_>>>()?
            }

            (None, Order::Field(field, order)) => self
                .req_index(field)?
                .query_iter(txn.clone(), order)?
                .collect::<Result<Vec<_>>>()?,

            (Some(filter), Order::Primary(order)) => {
                let sel = filter.apply(&txn, &self)?;

                if sel.inv {
                    sel.filter(PrimaryIterator::new(txn.clone(), self.clone(), order)?)
                        .collect::<Result<Vec<_>>>()?
                } else {
                    let mut ids = sel.ids.into_iter().collect::<Vec<_>>();
                    ids.sort_unstable_by(if order == OrderKind::Asc {
                        order_primary_asc
                    } else {
                        order_primary_desc
                    });
                    ids
                }
            }

            (Some(filter), Order::Field(field, order)) => filter
                .apply(&txn, &self)?
                .filter(self.req_index(field)?.query_iter(txn.clone(), order)?)
                .collect::<Result<Vec<_>>>()?,
        };

        DocumentsIterator::new(handle.storage.clone(), self.clone(), ids)
    }

    /// Find documents using optional filter and ordering
    ///
    /// When none filter specified then all documents will be found.
    ///
    /// The vector with found documents will be returned.
    pub fn find_all<T: DeserializeOwned + Document>(
        &self,
        filter: Option<Filter>,
        order: Order,
    ) -> Result<Vec<T>> {
        self.find(filter, order)?.collect::<Result<Vec<_>>>()
    }

    pub fn find_ids(&self, filter: Option<Filter>) -> Result<HashSet<Primary>> {
        let handle = self.handle();

        let txn = Arc::new(ReadTransaction::new(handle.storage.clone())?);

        if let Some(filter) = filter {
            let sel = filter.apply(&txn, &self)?;
            if !sel.inv {
                Ok(sel.ids)
            } else {
                PrimaryIterator::new(txn, self.clone(), OrderKind::default())?
                    .filter(move |res| if let Ok(id) = res { sel.has(id) } else { true })
                    .collect::<Result<HashSet<_>>>()
            }
        } else {
            PrimaryIterator::new(txn, self.clone(), OrderKind::default())?
                .collect::<Result<HashSet<_>>>()
        }
    }

    /// Update documents using optional filter and modifier
    ///
    /// *Note*: When none filter specified then all documents will be modified.
    ///
    /// Returns the number of affected documents.
    ///
    pub fn update(&self, filter: Option<Filter>, modify: Modify) -> Result<usize> {
        let handle = self.handle();

        let found_ids = self.find_ids(filter)?;

        let mut count = 0;
        {
            let txn = WriteTransaction::new(handle.storage.clone())?;
            let f = PutFlags::empty();
            {
                for id in found_ids {
                    let (old_doc, new_doc) = {
                        let mut access = txn.access();
                        let old_doc =
                            RawDocument::from_bin(access.get(&handle.db, &Unaligned::new(id))?)?
                                .with_id(id);
                        let new_doc = RawDocument::new(modify.apply(old_doc.clone().into_inner()))
                            .with_id(id);

                        access
                            .put(&handle.db, &Unaligned::new(id), &new_doc.to_bin()?, f)
                            .wrap_err()?;

                        (old_doc, new_doc)
                    };

                    self.update_indexes(&txn, Some(&old_doc), Some(&new_doc))?;

                    count += 1;
                }
            }

            txn.commit().wrap_err()?;
        }

        Ok(count)
    }

    /// Remove documents using optional filter
    ///
    /// *Note*: When none filter specified then all documents will be removed.
    ///
    /// Returns the number of affected documents.
    ///
    pub fn remove(&self, filter: Option<Filter>) -> Result<usize> {
        let handle = self.handle();

        let found_ids = self.find_ids(filter)?;

        let mut count = 0;
        {
            let txn = WriteTransaction::new(handle.storage.clone())?;
            {
                for id in found_ids {
                    let old_doc = {
                        let mut access = txn.access();
                        let old_doc =
                            RawDocument::from_bin(access.get(&handle.db, &Unaligned::new(id))?)?
                                .with_id(id);

                        access.del_key(&handle.db, &Unaligned::new(id)).wrap_err()?;

                        old_doc
                    };

                    self.update_indexes(&txn, Some(&old_doc), None)?;

                    count += 1;
                }
            }

            txn.commit().wrap_err()?;
        }

        Ok(count)
    }

    /// Dump all documents which stored into the collection
    #[inline]
    pub fn dump<T: DeserializeOwned + Document>(&self) -> Result<DocumentsIterator<T>> {
        self.find(None, Order::default())
    }

    /// Load new documents into the collection
    ///
    /// *Note*: The old documents will be removed.
    ///
    pub fn load<T: Serialize + Document, I>(&self, docs: I) -> Result<usize>
    where
        I: IntoIterator<Item = T>,
    {
        self.purge()?;

        let handle = self.handle();

        let txn = WriteTransaction::new(handle.storage.clone())?;
        let f = PutFlags::empty();
        let mut count = 0;

        {
            for doc in docs.into_iter() {
                let doc = RawDocument::from_doc(&doc)?;
                let id = doc.req_id()?;

                {
                    let mut access = txn.access();

                    access
                        .put(&handle.db, &Unaligned::new(id), &doc.to_bin()?, f)
                        .wrap_err()?;
                }

                self.update_indexes(&txn, None, Some(&doc))?;

                count += 1;
            }
        }

        txn.commit().wrap_err()?;

        Ok(count)
    }

    /// Remove all documents from the collection
    ///
    pub fn purge(&self) -> Result<()> {
        let handle = self.handle();

        let txn = WriteTransaction::new(handle.storage.clone()).wrap_err()?;
        let mut access = txn.access();

        let indexes = handle.indexes.read().wrap_err()?;
        for index in indexes.iter() {
            index.purge(&mut access)?;
        }

        access.clear_db(&handle.db).wrap_err()
    }

    /// Checks the collection contains document with specified primary key
    pub fn has(&self, id: Primary) -> Result<bool> {
        let handle = self.handle();

        let txn = ReadTransaction::new(handle.storage.clone()).wrap_err()?;
        let access = txn.access();

        access
            .get::<Unaligned<Primary>, [u8]>(&handle.db, &Unaligned::new(id))
            .to_opt()
            .map(|res| res != None)
            .wrap_err()
    }

    /// Get document from collection using primary key/identifier
    pub fn get<T: DeserializeOwned + Document>(&self, id: Primary) -> Result<Option<T>> {
        let handle = self.handle();

        let txn = ReadTransaction::new(handle.storage.clone()).wrap_err()?;
        let access = txn.access();

        Ok(
            match access
                .get::<Unaligned<Primary>, [u8]>(&handle.db, &Unaligned::new(id))
                .to_opt()
                .wrap_err()?
            {
                Some(val) => Some(RawDocument::from_bin(val)?.with_id(id).into_doc()?),
                None => None,
            },
        )
    }

    /// Replace document in the collection
    ///
    /// *Note*: The document must have primary key/identifier.
    ///
    pub fn put<T: Serialize + Document>(&self, doc: T) -> Result<()> {
        self.put_raw(RawDocument::from_doc(&doc)?)
    }

    fn put_raw(&self, doc: RawDocument) -> Result<()> {
        let id = doc.req_id()?;

        let handle = self.handle();

        let txn = WriteTransaction::new(handle.storage.clone()).wrap_err()?;

        let old_doc = {
            let mut access = txn.access();
            let old_doc =
                if let Some(old_doc) = access.get(&handle.db, &Unaligned::new(id)).to_opt()? {
                    Some(RawDocument::from_bin(old_doc)?.with_id(id))
                } else {
                    None
                };

            access
                .put(
                    &handle.db,
                    &Unaligned::new(id),
                    &doc.to_bin()?,
                    PutFlags::empty(),
                )
                .wrap_err()?;

            old_doc
        };

        self.update_indexes(
            &txn,
            if let Some(ref doc) = old_doc {
                Some(&doc)
            } else {
                None
            },
            Some(&doc),
        )?;

        txn.commit().wrap_err()?;

        Ok(())
    }

    /// Delete document with specified primary key/identifier from the collection
    pub fn delete(&self, id: Primary) -> Result<bool> {
        let handle = self.handle();

        let txn = WriteTransaction::new(handle.storage.clone()).wrap_err()?;

        let old_doc = {
            let mut access = txn.access();

            let old_doc =
                if let Some(old_doc) = access.get(&handle.db, &Unaligned::new(id)).to_opt()? {
                    RawDocument::from_bin(old_doc)?.with_id(id)
                } else {
                    // document not exists
                    return Ok(false);
                };

            access.del_key(&handle.db, &Unaligned::new(id)).wrap_err()?;

            old_doc
        };

        let status = self.update_indexes(&txn, Some(&old_doc), None)?;

        txn.commit().wrap_err()?;

        Ok(status)
    }

    fn update_indexes(
        &self,
        txn: &WriteTransaction,
        old_doc: Option<&RawDocument>,
        new_doc: Option<&RawDocument>,
    ) -> Result<bool> {
        let handle = self.handle();

        {
            let indexes = handle.indexes.read().wrap_err()?;
            let mut access = txn.access();

            for index in indexes.iter() {
                index.update_index(&mut access, old_doc, new_doc)?;
            }
        }

        Ok(old_doc.is_some())
    }

    /// Get the last primary key/identifier of inserted document
    pub fn last_id(&self) -> Result<Primary> {
        let handle = self.handle();

        let txn = ReadTransaction::new(handle.storage.clone()).wrap_err()?;
        let mut cursor = txn.cursor(self.clone()).wrap_err()?;
        let access = txn.access();

        cursor
            .last::<Unaligned<Primary>, [u8]>(&access)
            .to_opt()
            .map(|res| res.map(|(key, _val)| key.get()).unwrap_or(0))
            .wrap_err()
    }

    /// Get the new primary key/identifier
    pub fn new_id(&self) -> Result<Primary> {
        self.last_id().map(|id| id + 1)
    }

    /// Get indexes info from the collection
    pub fn get_indexes(&self) -> Result<KeyFields> {
        let handle = self.handle();

        let indexes = handle.indexes.read().wrap_err()?;
        Ok(indexes.iter().map(Index::field).collect::<Vec<_>>().into())
    }

    /// Set indexes of collection
    ///
    /// This method overrides collection indexes
    pub fn set_indexes<T, I: AsRef<[T]>>(&self, indexes: I) -> Result<()>
    where
        T: Clone,
        KeyField: From<T>,
    {
        for key_field in indexes.as_ref() {
            let KeyField { path, kind, key } = KeyField::from(key_field.clone());
            self.ensure_index(path, kind, key)?;
        }
        Ok(())
    }

    /// Set indexes using document type
    ///
    /// This method overrides collection indexes
    pub fn index<T: Document>(&self) -> Result<()> {
        self.set_indexes(T::key_fields())
    }

    /// Ensure index for the collection
    pub fn ensure_index<P: AsRef<str>>(
        &self,
        path: P,
        kind: IndexKind,
        key: KeyType,
    ) -> Result<bool> {
        if let Some(index) = self.get_index(&path)? {
            if index.kind() == kind && index.key() == key {
                return Ok(false);
            } else {
                self.drop_index(&path)?;
            }
        }

        self.create_index(&path, kind, key)
    }

    /// Checks the index for specified field exists for the collection
    pub fn has_index<P: AsRef<str>>(&self, path: P) -> Result<bool> {
        let path = path.as_ref();

        let handle = self.handle();

        let indexes = handle.indexes.read().wrap_err()?;

        Ok(indexes.iter().any(|index| index.path() == path))
    }

    /// Create index for the collection
    pub fn create_index<P: AsRef<str>>(
        &self,
        path: P,
        kind: IndexKind,
        key: KeyType,
    ) -> Result<bool> {
        let path = path.as_ref();

        let handle = self.handle();

        {
            let indexes = handle.indexes.read().wrap_err()?;
            // search alive index
            if indexes.iter().any(|index| index.path() == path) {
                return Ok(false);
            }
        }

        // create new index
        let index = Index::new(
            handle.storage.clone(),
            handle
                .storage
                .enumerate(IndexDef::new(handle.name.clone(), path, kind, key)),
        )?;

        {
            // fulfill index
            let txn = WriteTransaction::new(handle.storage.clone()).wrap_err()?;
            {
                let mut access = txn.access();

                let txn2 = ReadTransaction::new(handle.storage.clone()).wrap_err()?;
                let cursor2 = txn2.cursor(self.clone()).wrap_err()?;
                let access2 = txn2.access();

                for res in CursorIter::new(
                    MaybeOwned::Owned(cursor2),
                    &access2,
                    |c, a| c.first(a),
                    Cursor::next::<Unaligned<Primary>, [u8]>,
                )
                .wrap_err()?
                {
                    let (key, val) = res.wrap_err()?;
                    let doc = RawDocument::from_bin(val)?.with_id(key.get());
                    index.update_index(&mut access, None, Some(&doc))?;
                }
            }

            txn.commit().wrap_err()?;
        }

        // add index to collection indexes
        let mut indexes = handle.indexes.write().wrap_err()?;
        indexes.push(index);

        Ok(true)
    }

    /// Remove index from the collection
    pub fn drop_index<P: AsRef<str>>(&self, path: P) -> Result<bool> {
        let path = path.as_ref();

        let handle = self.handle();

        let found_pos = {
            let indexes = handle.indexes.read().wrap_err()?;
            indexes.iter().position(|index| index.path() == path)
        };

        Ok(if let Some(pos) = found_pos {
            let mut indexes = handle.indexes.write().wrap_err()?;
            let index = indexes.remove(pos);
            let txn = WriteTransaction::new(handle.storage.clone())?;
            let mut access = txn.access();
            index.to_delete(&mut access)?;
            true
        } else {
            false
        })
    }

    pub(crate) fn get_index<P: AsRef<str>>(&self, path: P) -> Result<Option<Index>> {
        let path = path.as_ref();

        let handle = self.handle();

        let indexes = handle.indexes.read().wrap_err()?;

        Ok(indexes
            .iter()
            .find(|index| index.path() == path)
            .map(Clone::clone))
    }

    pub(crate) fn req_index<P: AsRef<str>>(&self, path: P) -> Result<Index> {
        if let Some(index) = self.get_index(&path)? {
            Ok(index)
        } else {
            Err(format!("Missing index for field '{}'", path.as_ref())).wrap_err()
        }
    }

    pub(crate) fn to_delete(&self) -> Result<()> {
        let handle = self.handle();

        let txn = WriteTransaction::new(handle.storage.clone()).wrap_err()?;
        let mut access = txn.access();

        let indexes = handle.indexes.read().wrap_err()?;
        for index in indexes.iter() {
            index.purge(&mut access)?;
            index.to_delete(&mut access)?;
        }

        handle.delete.store(true, AtomicOrdering::SeqCst);
        access.clear_db(&handle.db).wrap_err()
    }
}

impl Drop for Collection {
    fn drop(&mut self) {
        let data = self.0.take().unwrap();

        if let Ok(CollectionData { db, delete, .. }) = Arc::try_unwrap(data) {
            if delete.load(AtomicOrdering::SeqCst) {
                if let Err(e) = db.delete() {
                    eprintln!("Error when deleting collection db: {}", e);
                }
            }
        }
    }
}

impl Deref for Collection {
    type Target = Database<'static>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        if let Some(data) = &self.0 {
            &data.db
        } else {
            unreachable!()
        }
    }
}

unsafe impl ConstDeref for Collection {
    type Target = Database<'static>;

    #[inline]
    fn const_deref(&self) -> &Self::Target {
        if let Some(data) = &self.0 {
            &data.db
        } else {
            unreachable!()
        }
    }
}

impl<'a> Into<Supercow<'a, Database<'a>>> for Collection {
    fn into(self) -> Supercow<'a, Database<'a>> {
        Supercow::shared(self)
    }
}

pub(crate) struct PrimaryIterator {
    txn: Arc<ReadTransaction<'static>>,
    cur: Cursor<'static, 'static>,
    order: OrderKind,
    init: bool,
}

impl PrimaryIterator {
    pub(crate) fn new(
        txn: Arc<ReadTransaction<'static>>,
        coll: Collection,
        order: OrderKind,
    ) -> Result<Self> {
        let cur = txn.cursor(coll)?;

        Ok(Self {
            txn,
            cur,
            order,
            init: false,
        })
    }
}

impl Iterator for PrimaryIterator {
    type Item = Result<Primary>;

    fn next(&mut self) -> Option<Self::Item> {
        let access = self.txn.access();
        match if self.init {
            match self.order {
                OrderKind::Asc => self.cur.next::<Unaligned<Primary>, [u8]>(&access),
                OrderKind::Desc => self.cur.prev::<Unaligned<Primary>, [u8]>(&access),
            }
        } else {
            self.init = true;
            match self.order {
                OrderKind::Asc => self.cur.first::<Unaligned<Primary>, [u8]>(&access),
                OrderKind::Desc => self.cur.last::<Unaligned<Primary>, [u8]>(&access),
            }
        }
        .to_opt()
        {
            Ok(Some((id, _val))) => Some(Ok(id.get())),
            Ok(None) => None,
            Err(e) => Some(Err(e).wrap_err()),
        }
    }
}

/// Iterator across found documents
///
/// You can use that to extract documents contents
///
/// The `DocumentsIterator::len()` method gets total number of found documents.
///
pub struct DocumentsIterator<T> {
    storage: Storage,
    coll: Collection,
    ids_iter: Box<dyn Iterator<Item = Primary> + Send>,
    phantom_doc: PhantomData<T>,
}

impl<T> DocumentsIterator<T> {
    pub(crate) fn new<I>(storage: Storage, coll: Collection, ids_iter: I) -> Result<Self>
    where
        I: IntoIterator<Item = Primary> + 'static,
        I::IntoIter: Send,
    {
        Ok(Self {
            storage,
            coll,
            ids_iter: Box::new(ids_iter.into_iter()),
            phantom_doc: PhantomData,
        })
    }
}

impl<T> Iterator for DocumentsIterator<T>
where
    T: DeserializeOwned + Document,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.ids_iter.next().map(|id| {
            let txn = ReadTransaction::new(self.storage.clone())?;
            {
                let access = txn.access();
                access
                    .get(&self.coll, &Unaligned::new(id))
                    .wrap_err()
                    .and_then(RawDocument::from_bin)
                    .map(|doc| doc.with_id(id))
                    .and_then(RawDocument::into_doc)
                    .wrap_err()
            }
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ids_iter.size_hint()
    }
}

impl<T> ExactSizeIterator for DocumentsIterator<T> where T: DeserializeOwned + Document {}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn order_primary_asc(a: &Primary, b: &Primary) -> Ordering {
    a.cmp(b)
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn order_primary_desc(a: &Primary, b: &Primary) -> Ordering {
    b.cmp(a)
}
