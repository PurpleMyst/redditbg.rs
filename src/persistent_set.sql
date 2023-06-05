CREATE TABLE IF NOT EXISTS PersistentSets (
    name TEXT NOT NULL,
    timestamp TEXT DEFAULT CURRENT_TIMESTAMP,
    url TEXT NOT NULL,

    PRIMARY KEY (name, url)
);
