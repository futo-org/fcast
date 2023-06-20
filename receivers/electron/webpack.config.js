const path = require('path');

module.exports = {
    mode: 'development',
    entry: './src/App.ts',
    target: 'electron-main',
    module: {
        rules: [
            {
                test: /\.tsx?$/,
                include: /src/,
                use: [{ loader: 'ts-loader' }]
            }
        ],
    },
    resolve: {
        extensions: ['.tsx', '.ts', '.js'],
    },
    output: {
        filename: 'bundle.js',
        path: path.resolve(__dirname, 'dist'),
    },
};