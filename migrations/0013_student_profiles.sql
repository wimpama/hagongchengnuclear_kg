CREATE TABLE kb_student_profiles (
    user_id TEXT PRIMARY KEY,
    student_no TEXT,
    name TEXT,
    class_name TEXT,
    college TEXT,
    major TEXT,
    grade TEXT,
    -- Authentication responses can be partial. Only an administrative profile
    -- lookup refreshes this timestamp, so a login cannot postpone enrichment.
    checked_at TEXT
);
