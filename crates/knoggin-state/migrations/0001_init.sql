-- SPDX-License-Identifier: AGPL-3.0-only
-- The state database on Postgres. `doc` is the sealed document.
CREATE TABLE knoggin_state (
  pk         text   COLLATE "C" NOT NULL,
  sk         text   COLLATE "C" NOT NULL,
  version    bigint NOT NULL,
  expires_ms bigint,
  doc        bytea  NOT NULL,
  PRIMARY KEY (pk, sk)
);
CREATE INDEX knoggin_state_expires ON knoggin_state (expires_ms) WHERE expires_ms IS NOT NULL;
