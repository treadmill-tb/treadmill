--
-- Mock values for testing purposes.
--

truncate table supervisors cascade;
truncate table users cascade;

-- insert a fake user
insert into users
(user_id, name, email, password_hash, created_at, last_login_at, locked)
values ('{2c71904b-a9b7-4e22-9d56-8a5d80567ccd}',
        'fake_admin_nologin',
        'example@example.com',
        '', -- no password_hash
        current_timestamp,
        current_timestamp, false);

-- insert a fake supervisor (
insert into supervisors
(supervisor_id, name,
 created_at, last_connected_at, created_by_admin_id,
 public_key,
 last_reported_status, last_reported_status_at,
 tags)
values ('{A8BA0AFB-6F6F-4CD6-A373-2E65417520DA}',
        'fake_supervisor_authtest',
        current_timestamp,
        current_timestamp,
        '{2c71904b-a9b7-4e22-9d56-8a5d80567ccd}',
        '-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAT/8zh6mwtqoJXFksvRc4KP3Pn+1nKEyfrcaJXtO8G2E=
-----END PUBLIC KEY-----
',
        '', -- no reported status
        current_timestamp,
        '{}');
