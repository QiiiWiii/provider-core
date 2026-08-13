DROP TABLE registration_codes;

CREATE TABLE invitations (
    token_hash BLOB PRIMARY KEY NOT NULL CHECK (length(token_hash) = 32),
    role TEXT NOT NULL CHECK (role IN ('super_admin', 'user')),
    expires_at INTEGER NOT NULL
);
