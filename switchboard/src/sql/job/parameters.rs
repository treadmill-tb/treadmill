use sqlx::PgExecutor;
use std::collections::HashMap;
use treadmill_rs::api::switchboard_supervisor::ParameterValue;

use super::JobId;

pub struct SqlJobParam {
    pub key: String,
    pub value: SqlJobParamValue,
}
#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.parameter_value")]
pub struct SqlJobParamValue {
    pub value: String,
    pub is_secret: bool,
}
impl From<SqlJobParamValue> for ParameterValue {
    fn from(value: SqlJobParamValue) -> Self {
        ParameterValue {
            value: value.value,
            secret: value.is_secret,
        }
    }
}

/// Reconstitute a [`HashMap`] of key-value pairs from the `job_parameters` table. Only selects
/// parameters associated with the specified `job_id`.
pub async fn fetch_by_job_id(
    job_id: JobId,
    conn: impl PgExecutor<'_>,
) -> Result<HashMap<String, ParameterValue>, sqlx::Error> {
    let records = sqlx::query_as!(
        SqlJobParam,
        r#"select key, value as "value:_" from tml_switchboard.job_parameters where job_id = $1;"#,
        job_id as JobId,
    )
    .fetch_all(conn)
    .await?;
    Ok(records
        .into_iter()
        .map(|record| {
            (
                record.key,
                ParameterValue {
                    value: record.value.value,
                    secret: record.value.is_secret,
                },
            )
        })
        .collect())
}

// /// Insert a set of key-value pairs into the `job_parameters` table with the specified `job_id`.
// ///
// /// To perform this action as part of a transaction, pass `transaction.as_mut()` as the connection
// /// parameter.
// pub async fn insert(
//     job_id: JobId,
//     parameters: HashMap<String, ParameterValue>,
//     conn: impl PgExecutor<'_>,
// ) -> Result<(), sqlx::Error> {
//     let (keys, values): (Vec<String>, Vec<ParameterValue>) = parameters.into_iter().unzip();
//     let values: Vec<SqlJobParamValue> = values
//         .into_iter()
//         .map(|ParameterValue { value, secret }| SqlJobParamValue {
//             value,
//             is_secret: secret,
//         })
//         .collect();

//     // since we have a uniform variable, we individually unnest the keys and values arrays
//     sqlx::query!(
//         // c_rec is a pseudotable of the form:
//         //  | unnest | value  | is_secret |
//         //  | (text) | (text) | (boolean) |
//         r#"
//         INSERT INTO tml_switchboard.job_parameters (job_id, key, value)
//         SELECT $1, (c_rec).unnest, row((c_rec).value, (c_rec).is_secret)::tml_switchboard.parameter_value
//         FROM UNNEST($2::text[], $3::tml_switchboard.parameter_value[]) as c_rec;
//         "#,
//         job_id as JobId,
//         keys.as_slice(),
//         values.as_slice() as &[SqlJobParamValue]
//     )
//     .execute(conn)
//     .await
//     .map(|_| ())
// }
