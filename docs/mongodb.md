[← Siesta](../README.md)

# MongoDB

A `.mongodb.js` file holds mongosh calls, as in a MongoDB playground.

```js
// @connection mongo-dev
// @database shop

// @name new orders
db.orders.find({ status: 'new' }, { customer: 1, total: 1 })
  .sort({ at: -1 })

db.orders.aggregate([{ $group: { _id: '$customer', n: { $sum: 1 } } }])

db.orders.updateOne({ _id: 7 }, { $set: { status: 'paid' } })
```

- **One call per statement.** A statement ends at `;` or at the end of a line, unless the next line starts with `.`.
- **Calls, not JavaScript.** Variables, functions and loops are errors. Pick the database with `// @database`, not `use`.
- **Literals:** bare keys, `'…'` strings, regexes, `ObjectId('…')`, `ISODate('…')`, `NumberLong`, `NumberDecimal`, `UUID`, `Timestamp`.
- Comments and directives start with `//`.

## Methods

| On a collection | |
|---|---|
| Read | `find` (with `.sort` `.limit` `.skip` `.projection` `.count()` `.explain()`), `findOne`, `aggregate`, `countDocuments`, `estimatedDocumentCount`, `distinct`, `getIndexes`, `stats` |
| Write | `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`, `findOneAndUpdate` / `Replace` / `Delete`, `createIndex`, `dropIndex(es)`, `drop`, `renameCollection` |

| On the database | |
|---|---|
| `db.` | `runCommand`, `adminCommand`, `getCollectionNames`, `getCollectionInfos`, `createCollection`, `dropDatabase`, `stats`, `version`, `serverStatus` |
| `show` | `dbs`, `collections`, `users` |

## Connection

```jsonc
// ~/.config/siesta/connections/mongo-dev.json
{ "driver": "mongodb", "url": "mongodb+srv://app@cluster0.example.net/shop" }
```

- A plain `mongodb://user:password@host:27017/db` URL fills the form's fields.
- `mongodb+srv://`, several hosts or options (`authSource`, `replicaSet`) stay as the URL. Its password moves to the encrypted password.
- `ssl.mode` sets TLS. `ssl.cert` is one PEM file with the certificate and key.

## Results

- Documents become rows. Nested documents and arrays stay JSON; an ObjectId shows its hex, a date its ISO time.
- Results open in the **JSON** tab. **Raw** shows the documents as extended JSON, with their types (`$oid`, `$date`, `$numberDecimal`).
- `find` and `aggregate` page on the server, unless the statement sets `.limit()` / `.skip()`. The column sort and filters go into the command.
- Writes report how many documents changed.

## Safety

- Reading methods, `show` and reading commands (`ping`, `find`, `count`, `listCollections`, …) are reads. Everything else writes.
- An `$out` or `$merge` stage makes an aggregation a write.
- `prod` and `read_only` work as for SQL. `read_only` is checked only in Siesta.
- Cancel drops the command; MongoDB may still finish it.

## Schema

The DB tab lists the database's collections, with fields seen in sample documents and their indexes. A double click inserts `db.<collection>.find({})`; **DDL** shows `db.createCollection(…)` and `createIndex(…)` calls.
