#!/bin/sh
# Runs once when the database volume is created. Ferrum must connect as an ordinary role:
# superusers bypass row-level security, and with it tenant isolation (the server refuses them).
set -eu
psql -v ON_ERROR_STOP=1 -v pw="$FERRUM_DB_PASSWORD" --username "$POSTGRES_USER" --dbname postgres <<'SQL'
create role ferrum login password :'pw';
create database ferrum owner ferrum;
SQL
