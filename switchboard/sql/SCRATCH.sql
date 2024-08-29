insert into tml_switchboard.job_parameters
select '{00000000-0000-4000-0000-000000000000}', key, value
from tml_switchboard.job_parameters
where job_id = '{10000000-0000-4000-0000-000000000000}';