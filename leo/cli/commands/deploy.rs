// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the Leo library.

// The Leo library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The Leo library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the Leo library. If not, see <https://www.gnu.org/licenses/>.

use super::*;
use aleo_std::StorageMode;
use snarkvm::{
    cli::helpers::dotenv_private_key,
    ledger::query::Query as SnarkVMQuery,
    package::Package as SnarkVMPackage,
    prelude::{
        deployment_cost,
        store::{helpers::memory::ConsensusMemory, ConsensusStore},
        PrivateKey,
        ProgramOwner,
        VM,
    },
};
use std::{path::PathBuf, str::FromStr};

/// Deploys an Aleo program.
#[derive(Parser, Debug)]
pub struct Deploy {
    #[clap(long, help = "Endpoint to retrieve network state from.", default_value = "http://api.explorer.aleo.org/v1")]
    pub endpoint: String,
    #[clap(flatten)]
    pub(crate) fee_options: FeeOptions,
    #[clap(long, help = "Disables building of the project before deployment.", default_value = "false")]
    pub(crate) no_build: bool,
    #[clap(long, help = "Enables recursive deployment of dependencies.", default_value = "false")]
    pub(crate) recursive: bool,
    #[clap(
        long,
        help = "Time in seconds to wait between consecutive deployments. This is to help prevent a program from trying to be included in an earlier block than its dependency program.",
        default_value = "12"
    )]
    pub(crate) wait: u64,
}

impl Command for Deploy {
    type Input = ();
    type Output = ();

    fn log_span(&self) -> Span {
        tracing::span!(tracing::Level::INFO, "Leo")
    }

    fn prelude(&self, context: Context) -> Result<Self::Input> {
        if !self.no_build {
            (Build { options: BuildOptions::default() }).execute(context)?;
        }
        Ok(())
    }

    fn apply(self, context: Context, _: Self::Input) -> Result<Self::Output> {
        // Get the program name.
        let project_name = context.open_manifest()?.program_id().to_string();

        // Get the private key.
        let private_key = match &self.fee_options.private_key {
            Some(key) => PrivateKey::from_str(key)?,
            None => PrivateKey::from_str(
                &dotenv_private_key().map_err(CliError::failed_to_read_environment_private_key)?.to_string(),
            )?,
        };

        // Specify the query
        let query = SnarkVMQuery::from(&self.endpoint);

        let mut all_paths: Vec<(String, PathBuf)> = Vec::new();

        // Extract post-ordered list of local dependencies' paths from `leo.lock`.
        if self.recursive {
            // Cannot combine with private fee.
            if self.fee_options.record.is_some() {
                return Err(CliError::recursive_deploy_with_record().into());
            }
            all_paths = context.local_dependency_paths()?;
        }

        // Add the parent program to be deployed last.
        all_paths.push((project_name, context.dir()?.join("build")));

        for (index, (name, path)) in all_paths.iter().enumerate() {
            // Fetch the package from the directory.
            let package = SnarkVMPackage::<CurrentNetwork>::open(path)?;

            println!("📦 Creating deployment transaction for '{}'...\n", &name.bold());

            // Generate the deployment
            let deployment = package.deploy::<CurrentAleo>(None)?;
            let deployment_id = deployment.to_deployment_id()?;

            // Generate the deployment transaction.
            let transaction = {
                // Initialize an RNG.
                let rng = &mut rand::thread_rng();

                let store =
                    ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(StorageMode::Production)?;

                // Initialize the VM.
                let vm = VM::from(store)?;

                // Compute the minimum deployment cost.
                let (minimum_deployment_cost, _) = deployment_cost(&deployment)?;

                // Prepare the fees.
                let fee = match &self.fee_options.record {
                    Some(record) => {
                        let fee_record = parse_record(&private_key, record)?;
                        let fee_authorization = vm.authorize_fee_private(
                            &private_key,
                            fee_record,
                            minimum_deployment_cost,
                            self.fee_options.priority_fee,
                            deployment_id,
                            rng,
                        )?;
                        vm.execute_fee_authorization(fee_authorization, Some(query.clone()), rng)?
                    }
                    None => {
                        let fee_authorization = vm.authorize_fee_public(
                            &private_key,
                            minimum_deployment_cost,
                            self.fee_options.priority_fee,
                            deployment_id,
                            rng,
                        )?;
                        vm.execute_fee_authorization(fee_authorization, Some(query.clone()), rng)?
                    }
                };
                // Construct the owner.
                let owner = ProgramOwner::new(&private_key, deployment_id, rng)?;

                // Create a new transaction.
                Transaction::from_deployment(owner, deployment, fee)?
            };
            println!("✅ Created deployment transaction for '{}'", name.bold());

            // Determine if the transaction should be broadcast, stored, or displayed to the user.
            handle_broadcast(
                &format!("{}/{}/transaction/broadcast", self.endpoint, self.fee_options.network),
                transaction,
                name,
            )?;

            if index < all_paths.len() - 1 {
                std::thread::sleep(std::time::Duration::from_secs(self.wait));
            }
        }

        Ok(())
    }
}
