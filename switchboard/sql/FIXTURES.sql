--
-- Mock values for testing purposes.
--

truncate table supervisors cascade;
truncate table users cascade;
truncate table api_tokens cascade;

-- insert a fake user
insert into users
(user_id, name, email, password_hash, user_type, created_at, last_login_at, locked)
values ('{2c71904b-a9b7-4e22-9d56-8a5d80567ccd}',
        'fake_admin1',
        'example@example.com',
           -- PASSWORD: FAKEFAKE
        '$argon2id$v=19$m=19456,t=2,p=1$3ZxJX8IBx77yc4cIO87UMg$z6Zvfi2uK7d0hreUSb3xZlLi8w3y1966ldxOqe2/O/4',
        'system',
        current_timestamp,
        current_timestamp, false);

insert into users
(user_id, name, email, password_hash, user_type, created_at, last_login_at, locked)
values ('{8752ca01-a650-4716-b0a1-d2f1860e4175}',
        'fake_user1',
        'fake_user1@example.com',
           -- PASSWORD: FAKEFAKE
        '$argon2id$v=19$m=19456,t=2,p=1$3ZxJX8IBx77yc4cIO87UMg$z6Zvfi2uK7d0hreUSb3xZlLi8w3y1966ldxOqe2/O/4',
        'normal',
        '2024-07-12 20:46:18.554751 +00:00',
        '2024-07-12 20:46:18.554766 +00:00',
        false);

-- insert a fake supervisor (
insert into supervisors
    (supervisor_id, name, last_connected_at, auth_token, tags)
values ('{7d55ec6d-15e7-4b84-8c04-7c085fe60df4}',
        'fake_supervisor_authtest',
        current_timestamp,
           -- Authorization: Bearer "OCkrhbDMiUG7rY1LlSfywBvgkqb1CyOt0djIgos9QDz6XyIaP+gYB62XJ6HK78ffPtvDVyi9bRj4Fj1xVVyFeixZPW0anU00Lzx3qckiP25Xt5cZbZTXxFKfb6ifHpFi83KwkGZYrsaVcXsf1Lc607CucHnSvZ9+uZUSnhrN4rc"
        '\x38292b85b0cc8941bbad8d4b9527f2c01be092a6f50b23add1d8c8828b3d403cfa5f221a3fe81807ad9727a1caefc7df3edbc35728bd6d18f8163d71555c857a2c593d6d1a9d4d342f3c77a9c9223f6e57b797196d94d7c4529f6fa89f1e9162f372b0906658aec695717b1fd4b73ad3b0ae7079d2bd9f7eb995129e1acde2b7',
        '{"supervisor:7d55ec6d-15e7-4b84-8c04-7c085fe60df4"}');

-- insert a fake token under fake_user1
INSERT INTO api_tokens
(token_id, token, user_id, inherits_user_permissions, canceled, created_at, expires_at)
VALUES ('3be73eea-192f-46c0-af01-92f574290c81',
           -- tml-api-token: B1oy2ko1wVdGKbvKc/9dKi7ggZYLTLzdm2As4CWV15fyuzvHsbBQOvnN+/RpB7OvVJjRYhldlSY4iFsNZq5XpO8fXiqRN6O/gn+nP5cA1J6ox2d2jV32TGzahTZAQZUFwIsI11Mye+Jus97L1e+l3O/0yBt/sywoJFFwkUVOFX8
        '\x075a32da4a35c1574629bbca73ff5d2a2ee081960b4cbcdd9b602ce02595d797f2bb3bc7b1b0503af9cdfbf46907b3af5498d162195d952638885b0d66ae57a4ef1f5e2a9137a3bf827fa73f9700d49ea8c767768d5df64c6cda853640419505c08b08d753327be26eb3decbd5efa5dceff4c81b7fb32c2824517091454e157f',
        '8752ca01-a650-4716-b0a1-d2f1860e4175',
        true,
        NULL,
        '2024-07-12 13:56:50.616829-07',
        '2124-07-12 13:56:50.616829-07');

INSERT INTO user_privileges
    (user_id, permission)
VALUES ('8752ca01-a650-4716-b0a1-d2f1860e4175', 'enqueue_ci_job');