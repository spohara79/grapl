
#[macro_use]
extern crate failure;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;

extern crate dgraph_rs;
extern crate graph_descriptions;
extern crate serde;
extern crate sqs_microservice;
extern crate grpc;

use std::collections::HashMap;
use std::time::SystemTime;

use failure::Error;
use graph_descriptions::graph_description::*;
use sqs_microservice::handle_s3_sns_sqs_proto;

use dgraph_rs::protos::api;
use dgraph_rs::protos::api_grpc::{DgraphClient, Dgraph};
use dgraph_rs::Transaction;
use dgraph_rs::protos::api_grpc;
use std::sync::Arc;

use grpc::{Client, ClientStub};
use grpc::ClientConf;

#[derive(Debug, Fail)]
enum MergeFailure {
    #[fail(display = "Transaction failure")]
    TransactionFailure
}

#[derive(Serialize, Deserialize, Debug)]
struct Uid {
    uid: String
}

#[derive(Serialize, Deserialize, Debug)]
struct DgraphResponse {
    response: HashMap<String, Vec<Uid>>,
}

struct DgraphQuery<'a> {
    node_key: &'a str,
    s_key: String,
    query: String,
    insert: serde_json::Value,
}

impl<'a> DgraphQuery<'a> {

    fn get_skey(&self) -> &str {
        &self.s_key
    }

    fn mut_insert(&mut self) -> &mut serde_json::Value {
        &mut self.insert
    }


    fn get_insert(&mut self) -> & serde_json::Value {
        &self.insert
    }
}


impl<'a> From<(usize, &'a NodeDescription)> for DgraphQuery<'a> {
    fn from((key, node): (usize, &'a NodeDescription)) -> DgraphQuery<'a> {
        let key = key as u16;
        let mut s_key = String::from("q");
        s_key.push_str(&key.to_string());

        let node_key = node.get_key();
        let query = format!(r#"
            {s_key}(func: eq(node_key, "{node_key}"))
            {{
                uid,
            }}
        "#, s_key=s_key, node_key=node_key);

        let mut insert = (*node).clone().into_json();

        for value in insert.as_object_mut().unwrap().values_mut() {
            if let Some(s_value) = value.clone().as_str() {
                let escaped_value = s_value.replace("\\", "\\\\")
                    .replace("\"", "\\\"")
                    .replace("\'", "\\\'")
                    .replace("\n", "\\\n")
                    .replace("\t", "\\\t");

                *value = serde_json::Value::from(escaped_value);
            }
        }

        DgraphQuery {
            node_key,
            s_key,
            query,
            insert
        }
    }
}

struct BulkUpserter<'a> {
    queries: Vec<DgraphQuery<'a>>,
    client: &'a DgraphClient,
    query_buffer: String,
    insert_buffer: String,
    node_key_to_uid: HashMap<String, String>
}

impl<'a> BulkUpserter<'a> {
    pub fn new(client: &'a DgraphClient, nodes: impl IntoIterator<Item=&'a NodeDescription>) -> BulkUpserter<'a> {
        let nodes = nodes.into_iter();
        let nodes_len = nodes.size_hint();
        let queries: Vec<_> = nodes.enumerate().map(DgraphQuery::from).collect();

        let buf_len: usize = queries.iter().map(|q| &q.query).map(String::len).sum();
        let buf_len = buf_len + queries.len();

        let query_buffer= String::with_capacity(buf_len + 3);
        let insert_buffer= String::with_capacity(buf_len + 3);

        BulkUpserter {
            queries,
            client,
            query_buffer,
            insert_buffer,
            node_key_to_uid: HashMap::with_capacity(nodes_len.1.unwrap_or(nodes_len.0))
        }
    }

    pub fn upsert_all(&mut self) -> Result<(), Error> {
        let mut txn = Transaction::new(&self.client);

        info!("Generating queries");
        // clear, and then fill, the internal query buffer
        self.generate_queries();

        info!("Querying all nodes");
        // Query dgraph for remaining nodes
        let query_response = self.query_all(&mut txn)?;

        info!("Generating inserts");
        // Generate upserts
        self.generate_insert(query_response)?;

        info!("Performing bulk upsert");
        // perform upsert
        println!("insert_buffer: {}", self.insert_buffer);

        let mut mutation = api::Mutation::new();

        mutation.set_json = self.insert_buffer.as_bytes().to_owned();

        let mut_res = txn.mutate(
            mutation
        )?;

        let txn_commit = txn.commit()?;
        // We need to take the newly created uids and add them to our map
        self.node_key_to_uid.extend(mut_res.uids);

        Ok(())
    }

    fn generate_queries(&mut self) {
        self.query_buffer.clear();
        let all_queries = &mut self.query_buffer;

        all_queries.push_str("{");
        self.queries.iter().for_each(|query| {
            all_queries.push_str(&query.query);
            all_queries.push_str(",");
        });
        all_queries.push_str("}");

    }

    fn query_all(&mut self, txn: &mut Transaction) -> Result<DgraphResponse, Error> {
        let resp = txn.query(
            &self.query_buffer[..]
        )?;

        Ok(DgraphResponse{response: serde_json::from_slice(&resp.json)?})
    }


    fn generate_insert(&mut self, response: DgraphResponse) -> Result<(), Error> {
        self.insert_buffer.clear();
        let insert_buffer = &mut self.insert_buffer;
        insert_buffer.push_str("[");
        for to_insert in &mut self.queries {
            let response = response.response.get(to_insert.get_skey());

            match response.map(Vec::as_slice) {
                Some([uid]) => {
                    self.node_key_to_uid.insert(to_insert.node_key.into(), uid.uid.clone());
                    to_insert.mut_insert()["uid"] = serde_json::Value::from(uid.uid.clone());

                },
                // If we get an empty response we just create the node
                Some([]) => {
                    let placeholder = format!("_:{}", to_insert.node_key);

                    to_insert.mut_insert()["uid"] = serde_json::Value::from(placeholder);
                },
                // We should never get more than a single uid back
                Some(uids) if uids.len() > 1 => bail!("Got more than one response"),
                // If we generate a query we should never *not* have it in a response
                None => bail!("Could not find response"),
                _ => unreachable!("until slice patterns improve")

            };

            let insert = &to_insert.get_insert().to_string();

            insert_buffer.push_str(insert);
            insert_buffer.push_str(",");
        }

        info!("popped {:#?}", insert_buffer.pop());
        if insert_buffer.is_empty() {
            bail!("insert_buffer empty");
        }
        insert_buffer.push_str("]");

        Ok(())
    }
}

fn insert_edges<'a>(client: &DgraphClient,
                edges: impl IntoIterator<Item=&'a EdgeDescription>,
                key_uid: &HashMap<String, String>) -> Result<(), Error> {

    if key_uid.is_empty() {
        warn!("key_uid is empty");
        return Ok(())
    }

    let bulk_insert = generate_bulk_edge_insert(edges, key_uid)?;

    let mut mutation = api::Mutation::new();
    mutation.commit_now = true;
    mutation.set_json = bulk_insert.into_bytes();


    info!("inesrt_edges {:?}", client.mutate(Default::default(), mutation).wait()?);

    Ok(())
}

fn generate_bulk_edge_insert<'a>(edges: impl IntoIterator<Item=&'a EdgeDescription>,
                             key_uid: &HashMap<String, String>) -> Result<String, Error> {

    if key_uid.is_empty() {
        bail!("key_uid must not be empty");
    }

    let edges = edges.into_iter();
    let edges_len = edges.size_hint();
    let edges_len = edges_len.1.unwrap_or(edges_len.0);
    let mut bulk_insert = String::with_capacity(48 * edges_len);

    bulk_insert.push_str("[");
    for edge in edges {
        let to = &key_uid
            .get(edge.to_neighbor_key.as_str());
        let from = &key_uid
            .get(edge.from_neighbor_key.as_str());

        // TODO: Add better logs, with actual identifiers
        let (to, from) = match (to, from) {
            (None, None) => {warn!("Failed to map node_key to uid for to and from"); continue},
            (_, None) => {warn!("Failed to map node_key to uid for from"); continue},
            (None, _) => {warn!("Failed to map node_key to uid for to"); continue},
            (Some(to), Some(from)) => (to, from),
        };

        let insert_statement = generate_edge_insert(&to, &from, &edge.edge_name);
        bulk_insert.push_str(&insert_statement);
        bulk_insert.push_str(",");
    }
    // Eat the trailing comma, replace with "]"
    bulk_insert.pop();
    if bulk_insert.is_empty() {
        bail!("Failed to generate any edge insertions")
    }
    bulk_insert.push_str("]");
    Ok(bulk_insert)
}

fn generate_edge_insert(to: &str, from: &str, edge_name: &str) -> String {
    json!({
        "uid": from,
        edge_name: {
            "uid": to
        }
    }).to_string()
}
//
//enum NodeType {
//    Process,
//    File,
//    OutboundNetwork,
//    InboundNetwork,
//    IpAddress,
//}
//
//struct OutputMetadata {
//    node_key: String,
//    uid: String,
//    node_type: NodeType,
//    was_created: bool,
//}
//
//struct MergeResults {
//    meta: Vec<OutputMetadata>,
//    earliest: u64,
//    latest: u64,
//}


fn with_retries<T>(mut f: impl FnMut() -> Result<T, Error>) -> Result<T, Error> {

    let max = 6;
    let mut cur = 0;
    loop {
        match f() {
            t @ Ok(_) => break t,
            Err(e) => {
                if cur == max {
                    return Err(e)
                } else {
                    error!("with_retries: {}", e);
                    cur += 1;
                    std::thread::sleep_ms(cur * 10);
                }
            }

        }
    }
}


fn main() {

//    let _: Result<(), Error> = (|| {
//        let mg_client = api_grpc::DgraphClient::with_client(
//            Arc::new(
//                Client::new_plain("db.mastergraph", 9080, ClientConf {
//                    ..Default::default()
//                })?
//            )
//        );
//
//        set_process_schema(&mg_client);
//        set_file_schema(&mg_client);
//        set_ip_address_schema(&mg_client);
//        set_connection_schema(&mg_client);
//        Ok(())
//    })();

    handle_s3_sns_sqs_proto(move |subgraph: GraphDescription| {
        println!("handling new subgraph");

        let mg_client = &api_grpc::DgraphClient::with_client(
            Arc::new(
                Client::new_plain("db.mastergraph", 9080, ClientConf {
                    ..Default::default()
                })?
            )
        );

        let mut upserter = BulkUpserter::new(
            &mg_client,
        subgraph.nodes.values()
        );

        // Even if some node upserts fail we should create edges for the ones that succeeded
        let upsert_res = with_retries(|| upserter.upsert_all());

        with_retries(|| {
            println!("inserting {} edges", subgraph.edges.values().fold(0, |mut acc, e| {acc += e.edges.len(); acc}));
            let edges = subgraph.edges.values().map(|e| &e.edges).flatten();
            insert_edges(&mg_client, edges, &upserter.node_key_to_uid)
        })?;

        // Retry any upserts that failed
        upsert_res
    }, move |_| {

        // Encode and compress results

        // TODO: Publish results to SNS
        Ok(())
    })

}

pub fn set_process_schema(client: &DgraphClient) {
    let mut op_schema = api::Operation::new();
    op_schema.drop_all = false;
    op_schema.schema = concat!(
       		"node_key: string @upsert @index(hash) .\n",
       		"pid: int @index(int) .\n",
       		"create_time: int @index(int) .\n",
       		"asset_id: string @index(hash) .\n",
       		"terminate_time: int @index(int) .\n",
       		"image_name: string @index(hash) @index(fulltext) .\n",
       		"arguments: string  @index(fulltext) .\n",
       		"bin_file: uid @reverse @index(fulltext) .\n",
       		"children: uid @reverse .\n",
       		"created_files: uid @reverse .\n",
            "deleted_files: uid @reverse .\n",
            "read_files: uid @reverse .\n",
            "wrote_files: uid @reverse .\n",
            "created_connection: uid @reverse .\n",
            "bound_connection: uid @reverse .\n",
        ).to_string();
    let _res = client.alter(Default::default(), op_schema).wait().expect("set schema");
}

pub fn set_file_schema(client: &DgraphClient) {
    let mut op_schema = api::Operation::new();
    op_schema.drop_all = false;
    op_schema.schema = r#"
       		node_key: string @upsert @index(hash) .
       		asset_id: string @index(hash) .
       		create_time: int @index(int) .
       		delete_time: int @index(int) .
       		path: string @index(hash) .
        "#.to_string();
    let _res = client.alter(Default::default(), op_schema).wait().expect("set schema");

}

pub fn set_ip_address_schema(client: &DgraphClient) {
    let mut op_schema = api::Operation::new();
    op_schema.drop_all = false;
    op_schema.schema = r#"
       		node_key: string @upsert @index(hash) .
       		last_seen: int @index(int) .
       		external_ip: string @index(hash) .
        "#.to_string();
    let _res = client.alter(Default::default(), op_schema).wait().expect("set schema");

}

pub fn set_connection_schema(client: &DgraphClient) {
    let mut op_schema = api::Operation::new();
    op_schema.drop_all = false;
    op_schema.schema = concat!(
       		"node_key: string @upsert @index(hash) .\n",
       		"create_time: int @index(int) .\n",
       		"ip: string @index(hash) .\n",
       		"port: string @index(hash) .\n",
       		// outbound connections have a `connection` edge to inbound connections
       		"connection: uid @reverse .\n",
       		// outbound connections have a `connection` edge to external ip addresses
       		"external_connection: uid @reverse .\n",
    ).to_string();
    let _res = client.alter(Default::default(), op_schema).wait().expect("set schema");

}

