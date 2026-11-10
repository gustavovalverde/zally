# Runbook: Upgrade a Wallet Database

## Purpose

A Zally release that moves to a newer `zcash_client_sqlite` migrates the wallet database the first time it opens it. The migrations are one-way. Rolling back to an older Zally afterwards is not supported; the way back is to restore a backup taken before the upgrade.

Moving from a release on `zcash_client_sqlite 0.22.0-rc.4` to one on `0.23.0` applies 18 wallet migrations. Most of them return `CannotRevert` when asked to roll back, and Zally has no rollback path.

## Steps

1. Stop every process that opens the wallet database.
2. Back up the database together with its write-ahead log files, from the same stopped state:

   ```sh
   backup_dir="/var/backups/zally/$(date -u +%Y%m%dT%H%M%SZ)"
   mkdir -p "$backup_dir"
   for file in wallet.db wallet.db-wal wallet.db-shm; do
     if [ -e "$file" ]; then cp -p "$file" "$backup_dir/"; fi
   done
   ```

3. Start the new release. Its first `open_or_create` applies the migrations. A failure surfaces as `StorageError::MigrationFailed` (`requires_operator`); restore the backup before retrying.
4. Check that the account ID, balance, and exposed addresses match what the old release reported, and that sync makes progress.

## Rolling back

Stop the wallet, replace `wallet.db`, `wallet.db-wal`, and `wallet.db-shm` with the backed-up files (remove any of them the backup does not contain), and start the older release.

Never point an older release at an upgraded database. It does not refuse it: its migrator skips migrations it does not know, so it opens the database and then runs against tables and data it does not maintain.

Do not use `Wallet::reset_to_birthday` to undo an upgrade. It rebuilds the derived chain state with the release that runs it, so the rebuilt database carries that release's schema. It keeps Zally's payment ledger (send idempotency records, dispense holds, retained finalized PCZTs, pending and reverted broadcasts, and the chain-event cursor), but it discards the transaction records `zcash_client_sqlite` keeps, and nothing in it can return the database to the older schema. Only the backup can.

Zally adds its own ledger tables (`ext_zally_*`) on open without a migration. A release that adds one creates it empty the first time it opens the database; an older release ignores tables it does not know.
