use std::{
	borrow::Cow,
	fmt::Debug,
	fs::File,
	io::BufReader,
	sync::Arc,
	time::{Duration, Instant},
};

use llm::{InferenceParameters, InferenceRequest, InferenceStats, OutputRequest, Prompt, TokenBias, TokenId, TokenUtf8Buffer};
use poly_bias::{
	json::{JsonBiaser, JsonSchema},
	sampler::TopPTopKBiased,
	Biaser, NullBiaser,
};

pub use llm::{InferenceFeedback, InferenceResponse};

use crate::{
	backend::{Backend, BackendStats},
	config::{BiaserConfig, TaskConfig},
	memory::Memory,
	sequence::{Sequence, SequenceSet},
	stats::InferenceStatsAdd,
	types::{GenerateError, PromptRequest},
};

pub struct BackendSession {
	pub(crate) model: Arc<Box<dyn llm::Model>>,
	pub(crate) memory: Option<Arc<Box<dyn Memory>>>,
	pub(crate) session: llm::InferenceSession,
	pub(crate) inference_parameters: InferenceParameters,
	pub(crate) task_config: TaskConfig,
	pub(crate) stats: Arc<BackendStats>,
	pub(crate) task_name: String,
	pub(crate) backend: Arc<Backend>,
}

impl Debug for BackendSession {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("BackendSession")
			.field("inference_parameters", &self.inference_parameters)
			.field("task_config", &self.task_config)
			.field("task_name", &self.task_name)
			.finish()
	}
}

impl BackendSession {
	fn reminder_prompt(&mut self, request: &PromptRequest) -> Result<Option<String>, GenerateError> {
		// Check if we need to recall items from memory first
		if let Some(memorization) = &self.task_config.memorization {
			if let Some(retrieve) = memorization.retrieve {
				if retrieve > 0 {
					// Calculate embedding for prompt
					let backend = self.backend.clone();
					let embedding = backend.embedding(&self.task_config.model, request)?;

					let handle = tokio::runtime::Handle::current();
					let _guard = handle.enter();
					let memory = self.memory.clone().unwrap();
					let reminder_prompt = handle
						.block_on(tokio::spawn(async move {
							let rm = memory.get(&embedding.embedding, retrieve);
							let remembered = rm.await?;
							tracing::debug!("retrieved from memory: {remembered:?}");
							let reminder_prompt: String = remembered.join("\n");
							Ok::<_, GenerateError>(reminder_prompt)
						}))
						.unwrap()?;
					tracing::info!("Reminder prompt: {reminder_prompt}");
					return Ok(Some(reminder_prompt));
				}
			}
		}
		Ok(None)
	}

	/// Perform a completion task following the task's configuration.
	pub fn complete(
		&mut self,
		request: &PromptRequest,
		callback: impl FnMut(InferenceResponse) -> Result<InferenceFeedback, GenerateError>,
	) -> Result<InferenceStats, GenerateError> {
		// Perform inference
		let stats = self.complete_actual(request, callback)?;
		let prompt_tokens_per_s = (stats.prompt_tokens as f64) / stats.feed_prompt_duration.as_secs_f64();
		let predict_tokens_per_s = (stats.predict_tokens as f64) / stats.predict_duration.as_secs_f64();

		tracing::info!(
			"completion finished; {prompt_tokens_per_s:.3} t/s prompt, {predict_tokens_per_s:.3} t/s predict; stats: {:?}",
			stats
		);
		self.stats.add(&self.task_name, &stats, self.inference_parameters.n_threads);

		// Perform memorization
		if let Some(memorization) = &self.task_config.memorization {
			if memorization.store_prompts {
				let backend = self.backend.clone();

				// Calculate embedding
				let embedding = backend.embedding(&self.task_config.model, request)?;

				// Commit to memory in the background
				let text = request.prompt.clone();
				let memory = self.memory.clone().unwrap();

				let handle = tokio::runtime::Handle::current();
				let _guard = handle.enter();
				handle
					.block_on(tokio::spawn(async move {
						memory.store(&text, &embedding.embedding).await?;
						tracing::debug!("committed to memory: {text}");
						Ok::<(), GenerateError>(())
					}))
					.unwrap()?;
			}
		}

		Ok(stats)
	}

	fn complete_actual(
		&mut self,
		request: &PromptRequest,
		mut callback: impl FnMut(InferenceResponse) -> Result<InferenceFeedback, GenerateError>,
	) -> Result<InferenceStats, GenerateError> {
		let mut completion_stats = InferenceStats::default();

		// Generate tokens (prefix + prompt + postfix)
		let beginning_of_sentence = self.model.bot_token_id().is_some() && self.session.n_past == 0;
		tracing::debug!(
			"beginning-of-text token is {:?}, beginning_of_sentence={beginning_of_sentence:?}",
			self.model.bot_token_id()
		);
		let mut tokens = vec![];

		// Append reminder tokens
		if let Some(reminder_prompt) = self.reminder_prompt(request)? {
			tokens.append(&mut Prompt::Text(&reminder_prompt).to_tokens(self.model.tokenizer(), beginning_of_sentence && tokens.is_empty())?)
		}

		// Append prefix tokens
		if let Some(ref prefix) = self.task_config.prefix {
			tokens.append(&mut Prompt::Text(prefix).to_tokens(self.model.tokenizer(), beginning_of_sentence && tokens.is_empty())?);
		}

		// Generate user prompt tokens
		let mut user_tokens = Prompt::Text(&request.prompt).to_tokens(self.model.tokenizer(), beginning_of_sentence && tokens.is_empty())?;

		// Check for private tokens in user prompt
		let private_tokens = self.task_config.private_tokens.clone().unwrap_or(vec![]);
		let private_token_ids: Vec<u32> = private_tokens
			.iter()
			.map(|token_str| {
				let toks = self.model.tokenizer().tokenize(token_str, false).unwrap();
				if toks.len() != 1 {
					panic!("invalid forbidden token configured: {token_str}");
				}
				toks[0].1
			})
			.collect();
		if !private_token_ids.is_empty() && user_tokens.iter().any(|t| private_token_ids.contains(t)) {
			return Err(GenerateError::IllegalToken);
		}
		tokens.append(&mut user_tokens);

		// Append postfix tokens
		if let Some(ref postfix) = self.task_config.postfix {
			tokens.append(&mut Prompt::Text(postfix).to_tokens(self.model.tokenizer(), beginning_of_sentence && tokens.is_empty())?);
		}

		tracing::trace!("prompt tokens: {tokens:?}");

		// Feed initial prompt
		let start = Instant::now();
		self.session.feed_prompt(
			self.model.as_ref().as_ref(),
			&InferenceParameters::default(),
			Prompt::Tokens(&tokens),
			&mut OutputRequest::default(),
			|_| -> Result<InferenceFeedback, GenerateError> { Ok(InferenceFeedback::Continue) },
		)?;
		completion_stats.add(&InferenceStats {
			feed_prompt_duration: Instant::now().duration_since(start),
			prompt_tokens: tokens.len(),
			predict_duration: Duration::ZERO,
			predict_tokens: 0,
		});

		// If a bias prompt is configured, let the model freely generate tokens, then feed the bias prompt and start
		// biased prompt generation. The tokens generated before the bias prompt is fed are not returned.
		let mut rng = rand::thread_rng();
		if let Some(ref bias_prompt) = self.task_config.bias_prompt {
			let stats = self.session.infer(
				self.model.as_ref().as_ref(),
				&mut rng,
				&InferenceRequest {
					prompt: Prompt::Tokens(&[]),
					parameters: &self.inference_parameters,
					maximum_token_count: self.task_config.max_tokens,
					play_back_previous_tokens: false,
				},
				&mut OutputRequest::default(),
				|r| -> Result<InferenceFeedback, GenerateError> {
					match r {
						InferenceResponse::SnapshotToken(_) => Ok(InferenceFeedback::Continue),
						InferenceResponse::PromptToken(_) => Ok(InferenceFeedback::Continue),
						InferenceResponse::InferredToken(t) => {
							// Save to transcript
							if tracing::enabled!(tracing::Level::DEBUG) {
								tokens.push(self.model.tokenizer().tokenize(&t, false).unwrap()[0].1);
							}
							tracing::trace!("Unbiased output token: {t}");
							Ok(InferenceFeedback::Continue)
						}
						InferenceResponse::EotToken => Ok(InferenceFeedback::Halt),
					}
				},
			)?;
			completion_stats.add(&stats);

			// Feed the bias prompt
			tracing::info!("feeding bias prompt: {bias_prompt}");
			if tracing::enabled!(tracing::Level::DEBUG) {
				tokens.extend(self.model.tokenizer().tokenize(bias_prompt, false).unwrap().iter().map(|x| x.1));
			}
			let start = Instant::now();
			self.session.feed_prompt(
				self.model.as_ref().as_ref(),
				&InferenceParameters::default(),
				Prompt::Text(bias_prompt.as_str()),
				&mut OutputRequest::default(),
				|_| -> Result<InferenceFeedback, GenerateError> { Ok(InferenceFeedback::Continue) },
			)?;
			completion_stats.add(&InferenceStats {
				feed_prompt_duration: Instant::now().duration_since(start),
				prompt_tokens: tokens.len(),
				predict_duration: Duration::ZERO,
				predict_tokens: 0,
			});
		}

		// Set up biaser
		let schema: Option<Cow<JsonSchema>>;
		let mut biaser: Box<dyn Biaser> = match self.task_config.biaser {
			Some(BiaserConfig::JsonSchema(ref schema)) => Box::new(JsonBiaser::new(schema)),
			Some(BiaserConfig::JsonSchemaFile(ref path)) => {
				let file = File::open(path).unwrap();
				let rdr = BufReader::new(file);
				schema = Some(Cow::Owned(serde_json::from_reader(rdr).expect("valid JSON schema in file")));
				Box::new(JsonBiaser::new(schema.as_ref().unwrap()))
			}
			None => Box::new(NullBiaser {}),
		};

		// Inference loop
		let mut result_buffer = TokenUtf8Buffer::new();
		let vocabulary = self.model.tokenizer();
		let eot_token = self.model.eot_token_id();
		let mut inference_params = self.inference_parameters.clone();
		let mut tokens_generated: usize = 0;
		let mut stop_sequences = if self.task_config.stop_sequences.is_empty() {
			None
		} else if self.task_config.biaser.is_some() {
			tracing::warn!(
				"a biaser is configured for task {}, therefore the stop sequences are ignored",
				self.task_name
			);
			None
		} else {
			Some(SequenceSet::new(
				self.task_config.stop_sequences.iter().map(|x| Sequence::new(x.clone())).collect(),
			))
		};

		loop {
			let mut biaser_bias = biaser.bias(vocabulary, eot_token);

			// Remove private tokens from biaser
			biaser_bias.retain_mut(|t| !private_token_ids.contains(&t.0));

			// If there is only one token positively biased, that will be the next token
			let out_token_id = if biaser_bias.len() == 1 && biaser_bias[0].1 > 0.0 {
				tracing::debug!("only one token in bias, that will be our next: {:?}", biaser_bias[0]);
				// Still need to feed it to our model!
				let only_possible_token = biaser_bias[0].0;
				if only_possible_token != self.model.eot_token_id() {
					let start = Instant::now();
					self.session.feed_prompt(
						self.model.as_ref().as_ref(),
						&inference_params,
						Prompt::Tokens(&[only_possible_token as TokenId]),
						&mut OutputRequest::default(),
						|_| -> Result<InferenceFeedback, GenerateError> { Ok(InferenceFeedback::Continue) },
					)?;
					completion_stats.add(&InferenceStats {
						feed_prompt_duration: Instant::now().duration_since(start),
						prompt_tokens: 1,
						predict_duration: Duration::ZERO,
						predict_tokens: 0,
					});
				}
				only_possible_token
			} else {
				// Sample a token using the model
				let sampler = TopPTopKBiased {
					bias_tokens: TokenBias::new(biaser_bias),
					temperature: self.task_config.temperature,
					top_k: self.task_config.top_k,
					top_p: self.task_config.top_p,
					repeat_penalty: self.task_config.repeat_penalty,
					repetition_penalty_last_n: self.task_config.repetition_penalty_last_n,
				};

				inference_params.sampler = Arc::new(sampler);

				let start = Instant::now();
				let Ok(out) = self
					.session
					.infer_next_token(self.model.as_ref().as_ref(), &inference_params, &mut OutputRequest::default(), &mut rng) else {
						break;
					};
				completion_stats.add(&InferenceStats {
					feed_prompt_duration: Duration::ZERO,
					prompt_tokens: 0,
					predict_duration: Instant::now().duration_since(start),
					predict_tokens: 1,
				});
				vocabulary.id(&out).unwrap()
			};

			tokens_generated += 1;

			// Save to transcript
			if tracing::enabled!(tracing::Level::DEBUG) {
				tokens.push(out_token_id);
			}

			// Check for end of text
			if out_token_id == eot_token {
				break;
			}

			// Advance biaser
			biaser.advance(vocabulary, out_token_id);

			// Add token to result
			tracing::trace!("token: {out_token_id}");
			if let Some(output) = result_buffer.push(&vocabulary.token(out_token_id as usize)) {
				tracing::trace!("text: {output}");

				if let Some(ref mut stop_sequences) = stop_sequences {
					if stop_sequences.advance(&output) {
						tracing::debug!("stop because stop sequence encountered");
						break;
					}
				}

				if !private_tokens.contains(&output) {
					// Swallow private tokens
					match callback(InferenceResponse::InferredToken(output))? {
						InferenceFeedback::Continue => {}
						InferenceFeedback::Halt => break,
					}
				}
			}

			// Stop once we have enough tokens (and not in biased mode, because then the biaser decides when we stop)
			if self.task_config.biaser.is_none() {
				if let Some(max_tokens) = self.task_config.max_tokens {
					if tokens_generated >= max_tokens {
						break;
					}
				}
			}
		}

		if tracing::enabled!(tracing::Level::DEBUG) {
			let decoded = self.model.tokenizer().decode(tokens, false);
			let txt = String::from_utf8_lossy(&decoded);
			tracing::debug!("full transcript (excluding prelude): {txt}");
		}
		Ok(completion_stats)
	}
}