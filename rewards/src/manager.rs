use crate::constants::PAGE_SIZE;
use crate::storage::{
    PoolRewardConfig, PoolRewardData, RewardsStorageTrait, Storage, UserRewardData,
};
use crate::Client;
use cast::u128 as to_u128;
use soroban_sdk::{Address, Env, Map};

pub struct Manager {
    env: Env,
    storage: Storage,
}

impl Manager {
    pub fn new(e: &Env, storage: Storage) -> Manager {
        Manager {
            env: e.clone(),
            storage,
        }
    }

    pub fn initialize(&self) {
        self.add_reward_inv(0, 0);
        self.storage.set_pool_reward_data(&PoolRewardData {
            block: 0,
            accumulated: 0,
            last_time: 0,
        });
        self.storage.set_pool_reward_config(&PoolRewardConfig {
            tps: 0,
            expired_at: 0,
        });
    }

    pub fn update_rewards_data(&self, total_shares: u128) -> PoolRewardData {
        let config = self.storage.get_pool_reward_config();
        let data = self.storage.get_pool_reward_data();
        let now = self.env.ledger().timestamp();

        // 1. config not expired - snapshot reward
        // 2. config expired
        //  2.a data before config expiration - snapshot reward for now, increase block and generate inv
        //  2.b data after config expiration - snapshot reward for config end, increase block, snapshot reward for now, don't increase block

        if now < config.expired_at {
            self.update_rewards_data_snapshot(now, &config, &data, total_shares)
        } else if data.last_time > config.expired_at {
            // todo: don't increase block
            self.create_new_rewards_data(
                0,
                total_shares,
                PoolRewardData {
                    block: data.block + 1,
                    accumulated: data.accumulated,
                    last_time: now,
                },
            )
        } else {
            self.update_rewards_data_catchup(now, &config, &data, total_shares)
        }
    }

    fn calculate_user_reward(&self, start_block: u64, end_block: u64, user_share: u128) -> u128 {
        let result = self.calculate_reward(start_block, end_block, true);
        (result) as u128 * user_share
    }

    pub fn update_user_reward(
        &self,
        pool_data: &PoolRewardData,
        user: &Address,
        user_balance_shares: u128,
    ) -> UserRewardData {
        return match self.storage.get_user_reward_data(user) {
            Some(user_data) => {
                if user_data.pool_accumulated == pool_data.accumulated {
                    // nothing accumulated since last update
                    return user_data;
                }

                if user_balance_shares == 0 {
                    // zero balance, no new reward
                    return self.create_new_user_data(&user, &pool_data, user_data.to_claim);
                }

                let reward = self.calculate_user_reward(
                    user_data.last_block + 1,
                    pool_data.block,
                    user_balance_shares,
                );
                // let new_reward =
                //     (pool_data.accumulated - user_data.pool_accumulated) * user_shares / total_shares;
                self.create_new_user_data(&user, &pool_data, user_data.to_claim + reward)
            }
            None => self.create_new_user_data(&user, &pool_data, 0),
        };
    }

    pub fn get_amount_to_claim(
        &self,
        user: &Address,
        total_shares: u128,
        user_balance_shares: u128,
    ) -> u128 {
        // update pool data & calculate reward
        self.user_reward_data(user, total_shares, user_balance_shares)
            .to_claim
    }

    pub fn claim_reward(
        &self,
        user: &Address,
        total_shares: u128,
        user_balance_shares: u128,
    ) -> u128 {
        // update pool data & calculate reward
        let UserRewardData {
            last_block,
            pool_accumulated,
            to_claim: reward_amount,
        } = self.user_reward_data(user, total_shares, user_balance_shares);

        // transfer reward
        let reward_token = self.storage.get_reward_token();
        Client::new(&self.env, &reward_token).transfer_from(
            &self.env.current_contract_address(),
            &self.storage.get_reward_storage(),
            &user,
            &(reward_amount as i128),
        );

        // set available reward to zero
        let new_data = UserRewardData {
            last_block,
            pool_accumulated,
            to_claim: 0,
        };
        self.storage.set_user_reward_data(user, &new_data);
        reward_amount
    }

    // private functions

    fn write_reward_inv_to_page(&self, pow: u32, start_block: u64, value: u64) {
        let page_number = start_block / PAGE_SIZE.pow(pow + 1);
        let mut page = match start_block % PAGE_SIZE.pow(pow + 1) {
            0 => Map::new(&self.env),
            _ => self.storage.get_reward_inv_data(pow, page_number),
        };
        page.set(start_block, value);
        if pow > 0 {
            // println!("writing {} -> {} (page {}, pow {})", start_block, start_block + PAGE_SIZE.pow(pow) - 1, page_number, pow);
        } else {
            // println!("writing {} (page {})", start_block, page_number);
        }
        self.storage.set_reward_inv_data(pow, page_number, &page);
    }

    fn calculate_reward(&self, start_block: u64, end_block: u64, use_max_pow: bool) -> u64 {
        // calculate result from start_block to end_block [...]
        // use_max_pow disabled during aggregation process
        //  since we don't have such information and can be enabled after
        let mut result = 0;
        let mut block = start_block;

        let mut max_pow = 0;
        for pow in 1..255 {
            if start_block + PAGE_SIZE.pow(pow) - 1 > end_block {
                break;
            }
            max_pow = pow;
        }

        while block <= end_block {
            if block % PAGE_SIZE == 0 {
                // check possibilities to skip
                let mut block_increased = false;
                let mut max_block_pow = 0;
                for i in (1..max_pow + 1).rev() {
                    if block % PAGE_SIZE.pow(i) == 0 {
                        max_block_pow = i;
                        break;
                    }
                }
                if !use_max_pow {
                    // value not precalculated yet
                    max_block_pow -= 1;
                }

                for l_pow in (1..max_block_pow + 1).rev() {
                    let next_block = block + PAGE_SIZE.pow(l_pow);
                    if next_block > end_block {
                        continue;
                    }

                    let page_number = block / PAGE_SIZE.pow(l_pow + 1);
                    // println!("skipping {} -> {} (page {}, pow {})", block, next_block, page_number, l_pow);
                    let page = self.storage.get_reward_inv_data(l_pow, page_number);
                    result += page.get(block).expect("unknown block");
                    block = next_block;
                    block_increased = true;
                    break;
                }
                if !block_increased {
                    // couldn't find shortcut, looks like we're close to the tail. go one by one
                    // println!("skipping {} -> {} (page {}, pow {})", block, block + 1, block / PAGE_SIZE, 0);
                    let page = self.storage.get_reward_inv_data(0, block / PAGE_SIZE);
                    result += page.get(block).expect("unknown block");
                    block += 1;
                }
            } else {
                // println!("skipping {} -> {} (page {}, pow {})", block, block + 1, block / PAGE_SIZE, 0);
                let page = self.storage.get_reward_inv_data(0, block / PAGE_SIZE);
                result += page.get(block).expect("unknown block");
                block += 1;
            }
        }
        result
    }

    fn add_reward_inv(&self, block: u64, value: u64) {
        // write zero level page first
        self.write_reward_inv_to_page(0, block, value);

        if (block + 1) % PAGE_SIZE == 0 {
            // page end, at least one aggregation should be applicable
            for pow in 1..255 {
                let aggregation_size = PAGE_SIZE.pow(pow);
                if (block + 1) % aggregation_size != 0 {
                    // aggregation level not applicable
                    break;
                }
                let agg_page_start = block - block % aggregation_size;
                let aggregation = self.calculate_reward(agg_page_start, block, false);
                self.write_reward_inv_to_page(pow, agg_page_start, aggregation);
            }
        }
    }

    fn update_reward_inv(&self, accumulated: u128, total_shares: u128) {
        let reward_per_share = if total_shares > 0 {
            accumulated / total_shares
        } else {
            0
        };

        let data = self.storage.get_pool_reward_data();
        self.add_reward_inv(data.block, reward_per_share as u64);
    }

    fn update_rewards_data_snapshot(
        &self,
        now: u64,
        config: &PoolRewardConfig,
        data: &PoolRewardData,
        total_shares: u128,
    ) -> PoolRewardData {
        let reward_timestamp = now;
        let generated_tokens = to_u128(reward_timestamp - data.last_time) * to_u128(config.tps);
        self.create_new_rewards_data(
            generated_tokens,
            total_shares,
            PoolRewardData {
                block: data.block + 1,
                accumulated: data.accumulated + generated_tokens,
                last_time: now,
            },
        )
    }

    fn create_new_rewards_data(
        &self,
        generated_tokens: u128,
        total_shares: u128,
        new_data: PoolRewardData,
    ) -> PoolRewardData {
        self.storage.set_pool_reward_data(&new_data);
        self.update_reward_inv(generated_tokens, total_shares);
        new_data
    }

    fn update_rewards_data_catchup(
        &self,
        now: u64,
        config: &PoolRewardConfig,
        data: &PoolRewardData,
        total_shares: u128,
    ) -> PoolRewardData {
        let reward_timestamp = config.expired_at;

        let generated_tokens = to_u128(reward_timestamp - data.last_time) * to_u128(config.tps);
        let catchup_data = PoolRewardData {
            block: data.block + 1,
            accumulated: data.accumulated + generated_tokens,
            last_time: config.expired_at,
        };
        self.create_new_rewards_data(generated_tokens, total_shares, catchup_data.clone());
        // todo: don't increase block when config not enabled thus keeping invariants list small
        self.create_new_rewards_data(
            0,
            total_shares,
            PoolRewardData {
                block: catchup_data.block + 1,
                accumulated: catchup_data.accumulated,
                last_time: now,
            },
        )
    }

    fn create_new_user_data(
        &self,
        user: &Address,
        pool_data: &PoolRewardData,
        to_claim: u128,
    ) -> UserRewardData {
        let new_data = UserRewardData {
            last_block: pool_data.block,
            pool_accumulated: pool_data.accumulated,
            to_claim,
        };
        self.storage.set_user_reward_data(user, &new_data);
        new_data
    }

    fn user_reward_data(
        &self,
        user: &Address,
        total_shares: u128,
        user_balance_shares: u128,
    ) -> UserRewardData {
        self.update_user_reward(
            &self.update_rewards_data(total_shares),
            user,
            user_balance_shares,
        )
    }
}
