# Lomora - a Local LLM Rating Toolbox

A simple web app to monitor local LLM model performance (LM Studio compatible).

## Requirements

- Python 3.9+
- (Optional) NVIDIA GPU with drivers for GPU metrics

## Getting Started

1. Clone the repository

   ```bash
   git clone https://github.com/marcuz-apl/lomora.git
   cd lomora
   ```

2. Install dependencies

   ```bash
   pip install -r requirements.txt
   ```

3. (Optional) Start LM Studio and load a model (default endpoint http://localhost:1234). Override with LMSTUDIO_URL.

4. Run the app

   ```bash
   python main.py
   ```

5. Open your browser and visit

   ```
   http://localhost:8501
   ```

## License

This project is licensed under the [MIT License](./LICENSE).
