const webpack = require('webpack');
const path = require('path');
const CopyWebpackPlugin = require("copy-webpack-plugin");

// Build issues:
// * 'development' mode breaks running the service on WebOS hardware... Must use 'production'.
// * Must use '--no-minify' when packaging since packaging would break otherwise...
const buildMode = 'production';
// const buildMode = 'development';

// const TARGET = 'electron';
const TARGET = 'webOS';
// const TARGET = 'tizenOS';

module.exports = [
    {
        mode: buildMode,
        entry: {
            main: './src/Main.ts',
        },
        target: 'node8.12',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: [path.resolve(__dirname, '../../common/web'), path.resolve(__dirname, 'src')],
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            alias: {
                'src': path.resolve(__dirname, 'src'),
                'modules': path.resolve(__dirname, 'node_modules'),
                'common': path.resolve(__dirname, '../../common/web'),
            },
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: '[name].js',
            path: path.resolve(__dirname, 'dist/main'),
        },
        plugins: [
            new CopyWebpackPlugin({
                patterns: [
                    { from: 'package.json', to: '[name][ext]' },
                    { from: 'services.json', to: '[name][ext]' },
                ],
            }),
            new webpack.DefinePlugin({
                TARGET: JSON.stringify(TARGET)
            })
        ]
    }
];
