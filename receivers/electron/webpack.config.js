const webpack = require('webpack');
const path = require('path');
const CopyWebpackPlugin = require("copy-webpack-plugin");
// const buildMode = 'production';
const buildMode = 'development';

const TARGET = 'electron';
// const TARGET = 'webOS';
// const TARGET = 'tizenOS';

module.exports = [
    {
        mode: buildMode,
        entry: './src/App.ts',
        target: 'electron-main',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: [path.resolve(__dirname, '../common/web'), path.resolve(__dirname, 'src')],
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            alias: {
                'src': path.resolve(__dirname, 'src'),
                'modules': path.resolve(__dirname, 'node_modules'),
                'common': path.resolve(__dirname, '../common/web'),
            },
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: 'bundle.js',
            path: path.resolve(__dirname, 'dist'),
        },
        plugins: [
            new CopyWebpackPlugin({
                patterns: [
                    // Common assets
                    {
                        from: '../common/assets/**',
                        to: './[path][name][ext]',
                        context: path.resolve(__dirname, '..', 'common'),
                        globOptions: { ignore: ['**/*.txt'] }
                    },
                    // Target assets
                    {
                        from: '**',
                        to: './assets/[path][name][ext]',
                        context: path.resolve(__dirname, 'assets'),
                        globOptions: { ignore: [] }
                    }
                ],
            }),
            new webpack.DefinePlugin({
                TARGET: JSON.stringify(TARGET)
            })
        ]
    },
    {
        mode: buildMode,
        entry: {
            preload: './src/main/Preload.ts',
            renderer: './src/main/Renderer.ts',
        },
        target: 'electron-renderer',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: [path.resolve(__dirname, '../common/web'), path.resolve(__dirname, 'src')],
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            alias: {
                'src': path.resolve(__dirname, 'src'),
                'modules': path.resolve(__dirname, 'node_modules'),
                'common': path.resolve(__dirname, '../common/web'),
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
                    {
                        from: '../common/web/main/common.css',
                        to: '[name][ext]',
                    },
                    {
                        from: './src/main/*',
                        to: '[name][ext]',
                        globOptions: { ignore: ['**/*.ts'] }
                    }
                ],
            }),
            new webpack.DefinePlugin({
                TARGET: JSON.stringify(TARGET)
            })
        ]
    },
    {
        mode: buildMode,
        entry: {
            preload: './src/player/Preload.ts',
            renderer: './src/player/Renderer.ts',
        },
        target: 'electron-renderer',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: [path.resolve(__dirname, '../common/web'), path.resolve(__dirname, 'src')],
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            alias: {
                'src': path.resolve(__dirname, 'src'),
                'modules': path.resolve(__dirname, 'node_modules'),
                'common': path.resolve(__dirname, '../common/web'),
            },
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: '[name].js',
            path: path.resolve(__dirname, 'dist/player'),
        },
        plugins: [
            new CopyWebpackPlugin({
                patterns: [
                    {
                        from: '../common/web/player/common.css',
                        to: '[name][ext]',
                    },
                    {
                        from: './src/player/*',
                        to: '[name][ext]',
                        globOptions: { ignore: ['**/*.ts'] }
                    }
                ],
            }),
            new webpack.DefinePlugin({
                TARGET: JSON.stringify(TARGET)
            })
        ]
    }
];