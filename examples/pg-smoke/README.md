# pg-smoke

A minimal Rune app whose SQL is PostgreSQL dialect: one `app.migration` with `UUID DEFAULT
gen_random_uuid()`, `JSONB`, `TEXT[]`, `NUMERIC`, `DOUBLE PRECISION`, `TIMESTAMPTZ DEFAULT now()`,
a partial unique index and a `CHECK`; routes doing `INSERT … ON CONFLICT … RETURNING` and
`SELECT … FOR UPDATE` inside `db::tx`, and a unique violation answering 409.

It cannot run on SQLite. Its suite needs a `postgres` build and a server whose role may
`CREATE DATABASE`:

```sh
PG_TEST_URL=postgres://user:pass@localhost/postgres \
  cargo run -p mintworks --features postgres -- test examples/pg-smoke
```

With `PG_TEST_URL` set, `mintworks test` creates a fresh framework database and a fresh app
database on that server for every case (`DB_URL`/`APP_DB_URL` for the case) and drops both after
it. Unset, it uses temp SQLite files, where this app fails on its first statement.
